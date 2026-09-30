//! Tests for `serve.rs`, moved out of it unchanged.

use super::*;

struct AdvancingWriter {
    now: Arc<Mutex<Instant>>,
    writes: usize,
}

impl Write for AdvancingWriter {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        *lock(&self.now) += Duration::from_millis(4);
        self.writes += 1;
        Ok(input.len().min(1))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl FrameWriter for AdvancingWriter {
    fn set_frame_write_timeout(&mut self, _timeout: Duration) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn partial_write_progress_cannot_reset_the_frame_deadline() {
    let start = Instant::now();
    let now = Arc::new(Mutex::new(start));
    let mut writer = AdvancingWriter {
        now: Arc::clone(&now),
        writes: 0,
    };
    let result = write_frame_before(&mut writer, b"four", Duration::from_millis(10), || {
        *lock(&now)
    });

    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(
        writer.writes, 3,
        "the fourth partial write began after expiry"
    );
}

/// Once the reader has declared the connection departed, a paced flow may finish the frame
/// already pulled by the writer but must not requeue itself. The production change that must
/// make this fail is unconditional flow requeue after a successful write, which can delay
/// `gone` by an entire million-record replay after a peer half-closes its write side.
#[test]
fn departed_connection_drops_a_flow_after_its_current_frame() {
    let (out, captured) = capture(ConnId(700));
    let pulls = Arc::new(AtomicU64::new(0));
    let seen = Arc::clone(&pulls);
    let drops = Arc::new(AtomicU64::new(0));
    let probe = FlowDropProbe(Arc::clone(&drops));
    let flow_out = out.clone();
    assert!(out.start_flow(move || {
        let _probe = &probe;
        let seq = seen.fetch_add(1, Ordering::SeqCst);
        flow_out.depart(Departure::Eof);
        Some(Frame::Notification(Notification::new(Event::NodePty {
            agent_id: marion_core::contract::AgentId("flow".into()),
            seq,
            mono_ns: 0,
            bytes: "paced".into(),
        })))
    }));

    assert!(
        captured.try_recv().is_ok(),
        "the already-pulled frame drains"
    );
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "the departed Flow remained queued after its current frame"
    );
    assert_eq!(
        captured.try_recv(),
        Err(TryRecvError::Empty),
        "a departed flow was requeued"
    );
    assert_eq!(pulls.load(Ordering::SeqCst), 1);
}

/// A reader that hands out exactly the chunks it was given — the pty S11 measured, and every
/// socket, in a form a test can be precise about.
struct Chunks(std::collections::VecDeque<Vec<u8>>);

impl Read for Chunks {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        match self.0.pop_front() {
            None => Ok(0),
            Some(c) => {
                let n = c.len().min(out.len());
                out[..n].copy_from_slice(&c[..n]);
                if n < c.len() {
                    self.0.push_front(c[n..].to_vec());
                }
                Ok(n)
            }
        }
    }
}

fn chunks(cs: &[&[u8]]) -> Lines<Chunks> {
    Lines::new(Chunks(cs.iter().map(|c| c.to_vec()).collect()))
}

/// **Peer credentials are read from the kernel, and a reading that failed is `Unknown`.**
///
/// Two rows, and the second is the one that cannot be faked. A connected socket answers with a
/// uid — this process's, because a `cargo test` cannot make a peer of any other user, which is
/// the stated limit of what is measurable here and is why `handler::root_spawn_authorized` is
/// pinned separately over the whole three-way predicate. A descriptor that is **not a socket**
/// makes the peer-credential read fail for a real kernel reason (`ENOTSOCK`), and that is the
/// row a build
/// which ignored the return code could not survive: `Peer::Uid(crate::socket::own_uid())` returned
/// unconditionally would look right on every socket in this workspace while handing root
/// creation to a peer nobody identified.
#[test]
fn a_peer_is_the_kernels_answer_and_an_unreadable_one_is_never_taken_as_permission() {
    use std::os::fd::AsRawFd;
    let (a, _b) = std::os::unix::net::UnixStream::pair().expect("a socket pair");
    assert_eq!(
        peer_of(&a),
        Peer::Uid(crate::socket::own_uid()),
        "a connected socket has a peer, and it is this test's own user"
    );

    let path = std::env::temp_dir().join(format!("marion-peer-notsock-{}", std::process::id()));
    let f = std::fs::File::create(&path).expect("a regular file");
    assert_eq!(
        peer_of_fd(f.as_raw_fd()),
        Peer::Unknown,
        "a peer-credential read on a non-socket fails, and a check that could not be made \
         must not \
         report the answer it would have liked"
    );
    drop(f);
    let _ = std::fs::remove_file(&path);
}

fn all(mut l: Lines<impl Read>) -> Vec<String> {
    let mut v = Vec::new();
    while let Some(line) = l.next_line().unwrap() {
        v.push(line);
    }
    v
}

#[test]
fn the_default_idle_grace_is_section_5_7s_configurable_five_minutes() {
    assert_eq!(DEFAULT_IDLE_GRACE, Duration::from_secs(300));
}

/// A handler that is willing to exit and counts how often the loop asks. Nothing about node
/// state is involved: the question here is entirely the accept loop's, which is the half of
/// §5.7 the handler's own tests cannot reach.
#[derive(Default)]
struct Leaver {
    eligible: AtomicBool,
    waived: AtomicBool,
    asked: AtomicU64,
    exiting: AtomicBool,
    ticks: AtomicU64,
    /// `Some` makes this a handler that says when it has changed and has nothing time-driven
    /// due — the shape `RegistryHandle` has — so the accept loop sleeps until woken.
    quiet: Option<Arc<crate::wake::Signal>>,
}

impl Handle for Leaver {
    fn call(&self, _c: ConnId, call: &Call, _o: &Outbound) -> Result<MethodResult, RpcError> {
        Err(RpcError::unimplemented(
            call.method().as_str(),
            "this fixture is about the accept loop, not about answering",
            "§5.7",
        ))
    }

    fn gone(&self, _c: ConnId, _g: &ClientGone, _w: &Departure) {}

    fn exiting(&self) -> bool {
        self.exiting.load(Ordering::SeqCst)
    }

    fn tick(&self) {
        self.ticks.fetch_add(1, Ordering::SeqCst);
    }

    fn next_deadline(&self) -> Option<Instant> {
        match self.quiet {
            Some(_) => None,
            None => Some(Instant::now() + LEGACY_TICK),
        }
    }

    fn changes(&self) -> Option<Arc<crate::wake::Signal>> {
        self.quiet.clone()
    }

    fn idle_exit_eligible(&self) -> bool {
        self.eligible.load(Ordering::SeqCst)
    }

    fn idle_exit_grace_waived(&self) -> bool {
        self.waived.load(Ordering::SeqCst)
    }

    fn begin_idle_exit(&self) -> bool {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.exiting.store(true, Ordering::SeqCst);
        true
    }
}

/// **A departure wakes an idle writer**, and the writer still sends what was queued before it —
/// a `session/quit` answer is queued ahead of its own `QuitCompleted` departure. The queue never
/// closes while a subscription holds a clone, so the departure is the only thing that can.
#[test]
fn a_departure_wakes_an_idle_writer_after_it_drains_what_was_queued() {
    let (write_half, peer) = UnixStream::pair().unwrap();
    let (tx, rx) = sync_channel(OUTBOUND_CAPACITY);
    let out = Outbound {
        conn: ConnId(9),
        peer: Peer::Unknown,
        peer_pid: None,
        tx,
        departed: Arc::new(Mutex::new(None)),
        shutdown: None,
    };
    let subscription = out.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        run_conn_writer(write_half, rx, out, Duration::from_secs(5));
        let _ = done_tx.send(());
    });
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        done_rx.try_recv().is_err(),
        "the writer waits for as long as the connection lives"
    );
    let answer = Frame::Notification(Notification::new(Event::NodeState {
        agent_id: marion_core::contract::AgentId("n".into()),
        state: marion_core::node::NodeState::Running,
        reap_state: marion_core::node::ReapState::Live,
        ts: marion_core::encoding::SystemTime::from_unix_millis(0),
    }));
    assert!(subscription.send(&answer));
    subscription.depart(Departure::QuitCompleted);
    done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the departure did not wake the writer");
    writer.join().unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(peer), &mut line).unwrap();
    assert_eq!(
        line,
        answer.to_line(),
        "what was queued before the departure went out"
    );
}

/// The cadence a non-quiet [`Leaver`] asks for: a handler that knows nothing about wakes and
/// wants to be ticked on a clock, which [`Handle::next_deadline`] lets it do.
const LEGACY_TICK: Duration = Duration::from_millis(5);

/// An idle grace long enough that it is never what runs a pass inside a test's bound.
const LONG_GRACE: Duration = Duration::from_secs(3600);

/// A server over a [`Leaver`] with [`Leaver::quiet`] set: nothing but a wake runs its loop.
fn quiet_server(
    tag: &str,
    grace: Duration,
) -> (Server, Arc<Leaver>, SocketPaths, std::path::PathBuf) {
    let dir = std::path::PathBuf::from(format!("/tmp/ms-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let paths = crate::socket::socket_paths(&dir, std::path::Path::new("/p"), 1);
    let Acquired::Serving(serving) = acquire(&paths).unwrap() else {
        panic!("nothing was listening")
    };
    let leaver = Arc::new(Leaver {
        quiet: Some(crate::wake::Signal::new()),
        ..Leaver::default()
    });
    let native = Arc::new(NativeBootstrapService::disabled(
        serving.canonical_project().to_path_buf(),
    ));
    let server = Server::start_with_native_handler(
        serving,
        Arc::clone(&leaver) as Arc<dyn Handle>,
        native,
        grace,
    );
    (server, leaver, paths, dir)
}

/// **An idle loop does not run.** With nothing due and nothing changed, the accept loop sits
/// in `poll` — the old loop ran a pass every 5 ms whether or not anything had happened — and a
/// notify on the handler's signal is what runs the next pass.
#[test]
fn a_handler_with_nothing_due_is_ticked_only_when_something_changes() {
    let (server, leaver, _paths, dir) = quiet_server("quiet-idle", LONG_GRACE);
    assert!(marion_testsupport::until(|| leaver
        .ticks
        .load(Ordering::SeqCst)
        >= 1));
    std::thread::sleep(Duration::from_millis(50));
    let before = leaver.ticks.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        leaver.ticks.load(Ordering::SeqCst),
        before,
        "the loop ran passes with nothing due and nothing changed"
    );
    leaver.quiet.as_ref().unwrap().notify();
    assert!(
        marion_testsupport::until(|| leaver.ticks.load(Ordering::SeqCst) > before),
        "a change did not wake the loop"
    );
    server.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A departure wakes the loop**: zero clients is half of §5.7's predicate, and a loop with no
/// timeout that did not learn of it would never start the grace at all.
#[test]
fn a_departing_client_wakes_the_idle_exit_without_waiting_for_a_deadline() {
    let (server, leaver, paths, dir) = quiet_server("quiet-depart", Duration::ZERO);
    let client = UnixStream::connect(paths.socket()).expect("dial the supervisor");
    // The connection is registered on its accept pass; the eligibility flip itself sends no
    // wake, so only the client clause is holding the exit once this is set.
    assert!(marion_testsupport::until(|| leaver
        .ticks
        .load(Ordering::SeqCst)
        >= 1));
    std::thread::sleep(Duration::from_millis(50));
    leaver.eligible.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        leaver.asked.load(Ordering::SeqCst),
        0,
        "one client is not zero"
    );
    drop(client);
    assert!(
        marion_testsupport::until(|| leaver.asked.load(Ordering::SeqCst) == 1),
        "the departure did not wake the loop"
    );
    server.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// **Stop wakes the loop**, rather than waiting for whatever deadline it is asleep on.
#[test]
fn stopping_a_quiet_server_does_not_wait_out_its_deadline() {
    let (server, leaver, _paths, dir) = quiet_server("quiet-stop", LONG_GRACE);
    assert!(marion_testsupport::until(|| leaver
        .ticks
        .load(Ordering::SeqCst)
        >= 1));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        server.stop();
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("stop waited on the accept loop's deadline");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **NC — §5.7's exit is zero clients, then the whole grace, then exactly one record.**
///
/// Three accept-loop failures, none of which any handler test can see. *Too early*: exiting
/// while the grace is still running, which is the "and my agents were gone" outcome for an
/// operator who was reconnecting. *Resumed rather than restarted*: a client that came and went
/// leaving the old clock running, so the next departure exits instantly. *Again*: continuing to
/// serve after the exit record, which makes §5.7's one record several and the supervisor's
/// departure a thing it announced but did not do.
#[test]
fn the_accept_loop_waits_out_the_grace_restarts_it_per_client_and_leaves_once() {
    const GRACE: Duration = Duration::from_millis(400);
    const SETTLE: Duration = Duration::from_millis(120);

    let dir = std::path::PathBuf::from(format!("/tmp/ms-idle-exit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let paths = crate::socket::socket_paths(&dir, std::path::Path::new("/p"), 1);
    let Acquired::Serving(serving) = acquire(&paths).unwrap() else {
        panic!("nothing was listening")
    };
    let leaver = Arc::new(Leaver::default());
    let server =
        Server::start_with_idle_grace(serving, Arc::clone(&leaver) as Arc<dyn Handle>, GRACE);

    leaver.eligible.store(true, Ordering::SeqCst);
    std::thread::sleep(SETTLE);
    assert_eq!(
        leaver.asked.load(Ordering::SeqCst),
        0,
        "the grace had not elapsed"
    );

    let client = UnixStream::connect(paths.socket()).expect("dial the supervisor");
    std::thread::sleep(GRACE + SETTLE);
    assert_eq!(
        leaver.asked.load(Ordering::SeqCst),
        0,
        "one connected client is not zero clients, however long it says nothing"
    );

    drop(client);
    std::thread::sleep(SETTLE);
    assert_eq!(
        leaver.asked.load(Ordering::SeqCst),
        0,
        "the departure starts a fresh grace rather than resuming the one before the client"
    );
    std::thread::sleep(GRACE);
    assert_eq!(
        leaver.asked.load(Ordering::SeqCst),
        1,
        "zero clients for the whole grace is the predicate"
    );

    std::thread::sleep(GRACE + SETTLE);
    assert_eq!(
        leaver.asked.load(Ordering::SeqCst),
        1,
        "the loop stopped once the exit was recorded, rather than asking again"
    );

    server.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// **NC — a client that *said* it was leaving is not made to wait as though it had vanished.**
///
/// §5.7's grace is argued for one case: *"it should outlast an operator closing one window to
/// open another"* — a wait for a client that never announced itself. §7.3 is the whole design's
/// insistence that an announcement and a disappearance are different events, and this is that
/// distinction spent on time rather than on nodes: an explicit `session/quit` skips the wait,
/// a dropped socket serves every millisecond of it.
///
/// The failure this rules out is not cosmetic. A grace of five minutes is what makes the
/// predicate safe for a supervisor whose starting client has not connected yet, so the wait
/// cannot simply be shortened for everyone — and without the waiver every `marion run` would
/// leave a process behind for five minutes after saying goodbye.
#[test]
fn an_explicit_quit_waives_the_grace_and_a_silent_departure_does_not() {
    const GRACE: Duration = Duration::from_secs(120);
    const SETTLE: Duration = Duration::from_millis(150);

    let dir = std::path::PathBuf::from(format!("/tmp/ms-waived-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let paths = crate::socket::socket_paths(&dir, std::path::Path::new("/p"), 1);
    let Acquired::Serving(serving) = acquire(&paths).unwrap() else {
        panic!("nothing was listening")
    };
    let leaver = Arc::new(Leaver::default());
    let server =
        Server::start_with_idle_grace(serving, Arc::clone(&leaver) as Arc<dyn Handle>, GRACE);

    // Eligible under §5.7's two clauses, and nobody said anything: two minutes to go.
    leaver.eligible.store(true, Ordering::SeqCst);
    std::thread::sleep(SETTLE);
    assert_eq!(
        leaver.asked.load(Ordering::SeqCst),
        0,
        "a departure marion could not read waits the whole grace"
    );

    leaver.waived.store(true, Ordering::SeqCst);
    assert!(
        until(|| leaver.asked.load(Ordering::SeqCst) == 1),
        "an explicit quit does not wait out a grace justified by clients that say nothing"
    );

    server.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// **NC — a torn frame is read once, when it completes: not twice, and not never.**
///
/// The three failures this rules out are the three a naive reader actually commits. *Never*: the
/// half-frame is dropped when the next `read` arrives. *Twice*: the buffer is not cleared, so
/// the completed frame is emitted again on the following read. *Garbage*: the fragment is parsed
/// as though a `read()` were a frame, which is precisely what §11 item 1 measured going wrong,
/// with 40% of pty reads carrying no frame boundary.
#[test]
fn a_frame_split_across_reads_is_delivered_once_when_it_completes() {
    let whole = br#"{"jsonrpc":"2.0","id":1,"method":"node/get","params":{"agent_id":"a"}}"#;
    for cut in [1, 10, whole.len() / 2, whole.len() - 1] {
        let (head, tail) = whole.split_at(cut);
        let mut l = chunks(&[head, tail, b"\n"]);
        // The fragment is not a frame, and asking again does not invent one.
        assert_eq!(l.pending(), 0);
        let got = l.next_line().unwrap();
        assert_eq!(
            got.as_deref(),
            Some(std::str::from_utf8(whole).unwrap()),
            "cut at {cut}"
        );
        assert_eq!(l.next_line().unwrap(), None, "and not a second time");
        assert_eq!(l.pending(), 0, "nothing is left over");
    }
}

/// One `read` may carry several frames, and it may carry several frames plus a fragment. Both
/// are the ordinary case on a socket, and a reader that assumed one read is one frame would lose
/// every frame after the first.
#[test]
fn one_read_carrying_several_frames_yields_all_of_them_in_order() {
    let mut l = chunks(&[b"{\"a\":1}\n{\"a\":2}\n{\"a\":3", b"}\n{\"a\":4}\n"]);
    let mut seen = Vec::new();
    while let Some(line) = l.next_line().unwrap() {
        seen.push(line);
    }
    assert_eq!(
        seen,
        ["{\"a\":1}", "{\"a\":2}", "{\"a\":3}", "{\"a\":4}"],
        "frames are delimited by the newline, never by the read boundary"
    );
}

/// **NC — a frame that never completes is never delivered.**
///
/// A client that dies mid-write leaves a fragment. Treating it as a frame would mean acting on a
/// request that was never finished — and §2's vocabulary contains `node/kill`.
#[test]
fn a_frame_the_client_never_finished_is_not_a_frame() {
    let mut l = chunks(&[br#"{"jsonrpc":"2.0","id":1,"method":"node/kill""#]);
    assert_eq!(l.next_line().unwrap(), None, "end of stream, not a frame");
    assert!(
        l.pending() > 0,
        "its existence is still observable, so a supervisor is never guessing about it"
    );
}

/// An empty stream, an empty line and a stream that is only newlines are three different
/// nothings, and only the first ends the reading.
#[test]
fn an_empty_line_is_a_frame_and_an_empty_stream_is_not() {
    assert_eq!(all(chunks(&[])), Vec::<String>::new());
    assert_eq!(all(chunks(&[b"\n\n"])), ["", ""]);
    assert_eq!(all(chunks(&[b"{}\n\n{}\n"])), ["{}", "", "{}"]);
}

/// A `\r\n` peer does not get its `\r` silently eaten: the frame is handed up as it arrived and
/// the JSON parser decides. Trimming here would mean this module quietly editing payloads.
#[test]
fn the_terminator_is_the_newline_and_only_the_newline() {
    assert_eq!(all(chunks(&[b"{}\r\n"])), ["{}\r"]);
}

/// A peer that never sends a newline is bounded rather than allowed to grow the supervisor's
/// heap without limit — §5.7's "MUST keep running" applied to memory.
#[test]
fn a_frame_that_never_ends_is_refused_rather_than_buffered_forever() {
    let big = vec![b'x'; MAX_FRAME_BYTES + 1];
    let mut l = chunks(&[&big]);
    assert_eq!(
        l.next_line().unwrap_err(),
        LineError::Oversize {
            bytes: MAX_FRAME_BYTES + 1
        }
    );
}

/// JSON is UTF-8 by definition, so bytes that are not are not a JSON error — they are a stream
/// marion cannot read as text, which is a different sentence and a different outcome.
#[test]
fn a_frame_that_is_not_utf8_is_named_as_such() {
    let mut l = chunks(&[&[0xff, 0xfe, b'\n']]);
    assert_eq!(l.next_line().unwrap_err(), LineError::NotUtf8);
}

// ---------------------------------------------------------------- the loop, over a real socket

use crate::socket::{Acquired, SocketPaths, acquire};
use marion_core::contract::AgentId;
use marion_core::encoding::Duration as EncDuration;
use marion_core::harness::Harness;
use marion_core::node::{NodeState, ReapState};
use marion_core::proto::result::{NodeGetResult, TreeSubscribeResult};
use marion_core::proto::{Method, NodeSummary, ReplayPoint};
use std::ffi::OsString;
use std::io::BufRead;
use std::os::unix::net::UnixStream;

/// A `Handle` that records what it was asked and hands back a fixed answer.
///
/// Deliberately not the registry: this file's subject is the transport, and a handler with real
/// state would make every failure ambiguous between the two.
#[derive(Default)]
struct Recorder {
    connected: Mutex<Vec<ConnId>>,
    calls: Mutex<Vec<(ConnId, Method)>>,
    gone: Mutex<Vec<(ConnId, ClientGone, Departure)>>,
    subs: Mutex<Vec<Outbound>>,
    inputs: Mutex<Vec<(ConnId, marion_core::proto::Input)>>,
    panic_on: Mutex<Option<Method>>,
    quit_ok: AtomicBool,
    gone_signal: Mutex<Option<SyncSender<Departure>>>,
}

fn a_node() -> NodeSummary {
    NodeSummary {
        widened: vec![],
        budget: None,
        changed: None,
        review_of: None,
        review: None,
        agent_id: AgentId("a".into()),
        parent_id: None,
        name: None,
        agent_type: "codex-impl".into(),
        harness: Harness::Codex,
        harness_version: None,
        depth: 0,
        state: NodeState::Idle,
        reap_state: ReapState::Live,
        timeout: EncDuration::from_secs(900),
        pane: false,
        started_at: None,
        ended_at: None,
        tokens: None,
        attention: None,
        endpoint: None,
        race: None,
        cancel: None,
        workflow: None,
    }
}

impl Handle for Recorder {
    fn connected(&self, conn: ConnId) {
        lock(&self.connected).push(conn);
    }

    fn call(&self, conn: ConnId, call: &Call, out: &Outbound) -> Result<MethodResult, RpcError> {
        lock(&self.calls).push((conn, call.method()));
        if *lock(&self.panic_on) == Some(call.method()) {
            panic!("a handler blew up");
        }
        match call {
            Call::NodeGet(_) => Ok(MethodResult::NodeGet(NodeGetResult {
                node: a_node(),
                detail: Default::default(),
            })),
            Call::TreeSubscribe(_) => {
                lock(&self.subs).push(out.clone());
                Ok(MethodResult::TreeSubscribe(TreeSubscribeResult {
                    nodes: vec![a_node()],
                    read_point: ReplayPoint {
                        records: 1,
                        src_seq: None,
                    },
                }))
            }
            Call::SessionQuit(_) if self.quit_ok.load(Ordering::SeqCst) => Ok(
                MethodResult::SessionQuit(marion_core::proto::result::SessionQuitResult {
                    outcome: marion_core::proto::QuitOutcome::Detached {
                        detached: vec![AgentId("a".into())],
                        gate_exposed: vec![AgentId("a".into())],
                        guidance: marion_core::proto::DetachGuidance {
                            reattach: "call tree/subscribe".into(),
                            stop_fleet: "call session/quit KillTree".into(),
                        },
                        supervisor: marion_core::proto::SupervisorDisposition::Resident(
                            marion_core::proto::ResidentReason::NonTerminalNode,
                        ),
                    },
                }),
            ),
            other => Err(RpcError::unimplemented(
                other.method().as_str(),
                "this fixture answers node/get and tree/subscribe only",
                "§2",
            )),
        }
    }

    fn input(&self, conn: ConnId, input: &marion_core::proto::Input) {
        lock(&self.inputs).push((conn, input.clone()));
    }

    fn gone(&self, conn: ConnId, gone: &ClientGone, why: &Departure) {
        lock(&self.subs).retain(|out| out.conn() != conn);
        lock(&self.gone).push((conn, gone.clone(), why.clone()));
        if let Some(signal) = lock(&self.gone_signal).as_ref() {
            let _ = signal.send(why.clone());
        }
    }
}

fn assert_pane_failure_closes_connection(conn: ConnId, why: Departure) {
    let rec = Arc::new(Recorder::default());
    let handle = Arc::clone(&rec) as Arc<dyn Handle>;
    let stopping = Arc::new(AtomicBool::new(false));
    let (mut client, server_half) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let worker = {
        let stopping = Arc::clone(&stopping);
        std::thread::spawn(move || serve_conn(conn, server_half, handle, &stopping))
    };
    let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
    send(
        &mut client,
        Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
        1,
    );
    assert!(matches!(read_frame(&mut reader), Frame::Response(_)));
    let out = lock(&rec.subs)
        .iter()
        .find(|out| out.conn() == conn)
        .expect("the response causally follows subscription registration")
        .clone();

    out.fail(why.clone());

    let mut tail = String::new();
    assert_eq!(
        reader.read_line(&mut tail).unwrap(),
        0,
        "producer failure must wake the blocking reader and close the peer"
    );
    worker.join().unwrap();
    assert!(
        lock(&rec.subs).iter().all(|out| out.conn() != conn),
        "gone must perform unlisten-like cleanup"
    );
    assert_eq!(
        lock(&rec.gone).as_slice(),
        &[(conn, ClientGone::SocketClosed, why)]
    );
}

#[test]
fn pane_failures_shutdown_the_peer_and_run_gone_cleanup() {
    assert_pane_failure_closes_connection(
        ConnId(701),
        Departure::PaneOverflow {
            agent_id: "pane-overflow".into(),
        },
    );
    assert_pane_failure_closes_connection(
        ConnId(702),
        Departure::PaneRetentionFailed {
            agent_id: "pane-retention".into(),
            error: "retained pane stream exceeded its limit".into(),
        },
    );
    assert_pane_failure_closes_connection(
        ConnId(703),
        Departure::PaneInputFailed {
            agent_id: "pane-input".into(),
            error: "opaque input evidence was refused".into(),
        },
    );
}

struct FlowDropProbe(Arc<AtomicU64>);

impl Drop for FlowDropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn a_nonreading_peer_times_out_one_flow_frame_and_releases_it() {
    let rec = Arc::new(Recorder::default());
    let (gone_tx, gone_rx) = sync_channel(1);
    *lock(&rec.gone_signal) = Some(gone_tx);
    let handle = Arc::clone(&rec) as Arc<dyn Handle>;
    let stopping = Arc::new(AtomicBool::new(false));
    let conn = ConnId(703);
    let (mut client, server_half) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let worker = {
        let stopping = Arc::clone(&stopping);
        std::thread::spawn(move || {
            serve_conn_with_frame_timeout(
                conn,
                server_half,
                handle,
                &stopping,
                Duration::from_millis(20),
            )
        })
    };
    let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
    send(
        &mut client,
        Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
        1,
    );
    assert!(matches!(read_frame(&mut reader), Frame::Response(_)));
    let out = lock(&rec.subs)[0].clone();
    let drops = Arc::new(AtomicU64::new(0));
    let probe = FlowDropProbe(Arc::clone(&drops));
    let payload = "x".repeat(8 * 1024 * 1024);
    assert!(out.start_flow(move || {
        let _probe = &probe;
        Some(Frame::Notification(Notification::new(Event::NodePty {
            agent_id: AgentId("blocked-flow".into()),
            seq: 0,
            mono_ns: 0,
            bytes: payload.clone(),
        })))
    }));

    let departure = gone_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the absolute frame deadline wakes serve_conn");
    let mut tail = Vec::new();
    reader
        .read_to_end(&mut tail)
        .expect("fatal timeout shuts down the peer after any buffered prefix");
    worker.join().unwrap();

    assert_eq!(
        departure,
        Departure::TooSlow {
            queued: OUTBOUND_CAPACITY
        }
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1, "the paced cursor leaked");
    assert!(lock(&rec.subs).is_empty(), "gone did not unlisten the flow");
}

#[test]
fn a_panicking_flow_fails_the_connection_and_releases_once() {
    let rec = Arc::new(Recorder::default());
    let handle = Arc::clone(&rec) as Arc<dyn Handle>;
    let stopping = Arc::new(AtomicBool::new(false));
    let conn = ConnId(704);
    let (mut client, server_half) = UnixStream::pair().unwrap();
    // A hang guard rather than a synchronization bound: it has to sit far above the worst
    // scheduling delay a loaded machine can put between the flow's panic and the shutdown it
    // causes, because a lapse here means the connection never failed, not that it was slow.
    client
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let worker = {
        let stopping = Arc::clone(&stopping);
        std::thread::spawn(move || serve_conn(conn, server_half, handle, &stopping))
    };
    let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
    send(
        &mut client,
        Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
        1,
    );
    assert!(matches!(read_frame(&mut reader), Frame::Response(_)));
    let out = lock(&rec.subs)[0].clone();
    let drops = Arc::new(AtomicU64::new(0));
    let probe = FlowDropProbe(Arc::clone(&drops));
    assert!(out.start_flow(move || -> Option<Frame> {
        let _probe = &probe;
        panic!("internal flow panic")
    }));

    // The peer's `Eof` is the only thing here that is waited *for*, and the socket's read
    // timeout bounds it so a flow panic that stopped failing the connection reports itself
    // instead of wedging the suite. Everything after it is waited *on*: a `start_flow` that
    // returned true has already queued the flow, so the writer's catch, the shutdown, the
    // writer's exit and `gone` are all work this connection's worker must finish before it
    // returns, and joining it is the release barrier. Timing that chain instead would time the
    // scheduler — under load the wakeups behind the writer's old 20 ms departure poll outlasted
    // a one-second bound, which read as a leak the code had not committed.
    let mut tail = String::new();
    assert!(
        matches!(reader.read_line(&mut tail), Ok(0)),
        "flow panic did not shut down the peer"
    );
    drop(out);
    worker.join().unwrap();

    assert_eq!(
        lock(&rec.gone).as_slice(),
        &[(
            conn,
            ClientGone::SocketClosed,
            Departure::OutboundFlowPanicked
        )]
    );
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "the flow leaked or double-dropped"
    );
    assert!(lock(&rec.subs).is_empty(), "gone did not unlisten the flow");
}

/// `registry.rs`'s helper: assert that something **happens**, never how long it takes.
fn until(mut cond: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    cond()
}

struct Fixture {
    dir: std::path::PathBuf,
    paths: SocketPaths,
    rec: Arc<Recorder>,
    server: Option<Server>,
}

impl Fixture {
    fn new(tag: &str) -> Fixture {
        // A short path, for the reason `socket.rs`'s tests spell out: `temp_dir()` on macOS
        // overruns the 103 bytes a socket path may occupy.
        let dir = std::path::PathBuf::from(format!("/tmp/ms-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = crate::socket::socket_paths(&dir, std::path::Path::new("/p"), 1);
        // `socket_paths` puts the socket under `<dir>/<hash>/`; that is the production layout
        // and the test uses it rather than inventing a second one.
        let Acquired::Serving(serving) = acquire(&paths).unwrap() else {
            panic!("nothing was listening")
        };
        let rec = Arc::new(Recorder::default());
        let server = Server::start(serving, Arc::clone(&rec) as Arc<dyn Handle>);
        Fixture {
            dir,
            paths,
            rec,
            server: Some(server),
        }
    }

    fn dial(&self) -> UnixStream {
        let s = UnixStream::connect(self.paths.socket()).expect("dial the supervisor");
        // A bound so a test that never receives fails loudly instead of hanging a suite. It is
        // never asserted on: every assertion below is about *what* arrived, not when.
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(s) = self.server.take() {
            s.stop();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn send(s: &mut UnixStream, call: Call, id: i64) {
    let f = Frame::Request(Request::new(RequestId::Number(id), call));
    s.write_all(f.to_line().as_bytes()).unwrap();
    s.flush().unwrap();
}

fn read_frame(r: &mut std::io::BufReader<UnixStream>) -> Frame {
    let mut line = String::new();
    let n = r.read_line(&mut line).expect("a frame arrives");
    assert!(n > 0, "the supervisor closed instead of answering");
    Frame::from_line(&line).expect("the supervisor emits well-formed frames")
}

/// **The main socket holds at most its limit of clients**: one more is closed at accept, before
/// it costs a thread, and a slot that frees is taken again.
#[test]
fn a_connection_past_the_limit_is_closed_and_a_freed_slot_is_reused() {
    let dir = std::path::PathBuf::from(format!("/tmp/ms-limit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let paths = crate::socket::socket_paths(&dir, std::path::Path::new("/p"), 1);
    let Acquired::Serving(serving) = acquire(&paths).unwrap() else {
        panic!("nothing was listening")
    };
    let rec = Arc::new(Recorder::default());
    let server = Server::start_with_client_limit(serving, Arc::clone(&rec) as Arc<dyn Handle>, 2);
    let answered = |s: &UnixStream| {
        // A closed connection may refuse the write as well as the read; either is "not served".
        let f = Frame::Request(Request::new(RequestId::Number(1), node_get("a")));
        if (&*s).write_all(f.to_line().as_bytes()).is_err() {
            return false;
        }
        let mut line = String::new();
        std::io::BufReader::new(s.try_clone().unwrap())
            .read_line(&mut line)
            .map(|n| n > 0)
            .unwrap_or(false)
    };
    let dial = || {
        let s = UnixStream::connect(paths.socket()).unwrap();
        // macOS refuses the option with EINVAL on a connection the server has already closed —
        // which is what a dial past the limit is — and a closed one reads EOF at once anyway.
        let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
        s
    };
    let first = dial();
    let second = dial();
    assert!(answered(&first) && answered(&second), "two fit");
    let third = dial();
    assert!(!answered(&third), "the third is closed rather than served");
    drop(first);
    assert!(until(|| answered(&dial())), "a freed slot is served again");
    server.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ordinary_connection_is_announced_exactly_once() {
    let fx = Fixture::new("connected-once");
    let mut client = fx.dial();
    let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
    send(&mut client, node_get("a"), 1);
    assert!(matches!(read_frame(&mut reader), Frame::Response(_)));
    assert_eq!(
        lock(&fx.rec.connected).as_slice(),
        &[ConnId(1)],
        "ordinary accept and prepared relay both announced the same connection"
    );
}

#[test]
fn prepared_connection_abort_cannot_lose_its_writer_wakeup() {
    let (client, server) = UnixStream::pair().unwrap();
    let rec = Arc::new(Recorder::default());
    let (waiting_tx, waiting_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    let prepared = PreparedClaimedConn::prepare_with_test_wait_hook(
        ConnId(8_701),
        &server,
        Arc::clone(&rec) as Arc<dyn Handle>,
        OUTBOUND_FRAME_WRITE_TIMEOUT,
        move || {
            waiting_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        },
    )
    .unwrap_or_else(|(_, departure)| panic!("connection preparation failed: {departure:?}"));
    let departure = prepared.out.as_ref().unwrap().clone();
    waiting_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("writer reached its gated wait");
    let (dropped_tx, dropped_rx) = sync_channel(1);
    std::thread::spawn(move || {
        drop(prepared);
        let _ = dropped_tx.send(());
    });
    assert!(
        until(|| departure.departed().is_some()),
        "prepared drop did not begin aborting the writer"
    );
    release_tx.send(()).unwrap();
    dropped_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("abort notification was lost before the writer waited");
    drop(client);
    drop(server);
}

#[test]
fn ordinary_json_rpc_rejects_copied_native_token_before_capability_lookup() {
    let dir = marion_testsupport::scratch("native-ordinary");
    let paths = crate::socket::socket_paths(&dir, std::path::Path::new("/p"), 1);
    let Acquired::Serving(serving) = acquire(&paths).unwrap() else {
        panic!("nothing was listening")
    };
    let recorder = Arc::new(Recorder::default());
    let native = Arc::new(NativeBootstrapService::disabled(
        paths.canonical_project().to_path_buf(),
    ));
    let server = Server::start_with_native_handler(
        serving,
        Arc::clone(&recorder) as Arc<dyn Handle>,
        Arc::clone(&native),
        DEFAULT_IDLE_GRACE,
    );
    let mut client = UnixStream::connect(paths.socket()).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    client
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":900,\"method\":\"native/bootstrap\",\"params\":{\"capability\":\"copied-secret-token\"}}\n",
        )
        .unwrap();
    client.flush().unwrap();
    let mut response = String::new();
    std::io::BufReader::new(client)
        .read_line(&mut response)
        .unwrap();
    let response: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["id"], 900);
    assert_eq!(
        response["error"]["code"],
        marion_core::proto::error::METHOD_NOT_FOUND
    );
    assert_eq!(native.capability_lookup_count(), 0);
    assert!(lock(&recorder.calls).is_empty());

    // The environment is the one wire surface the native context grew after this refusal was
    // written. Neither the exact env-bearing native frame nor a JSON spelling of it may reach
    // the capability authority over the ordinary socket. Lines on one connection are consumed
    // in order, so the answer to the trailing JSON line (or the connection closing on the
    // binary line) proves both were already handled when the counters are read.
    let context = crate::native_bootstrap::DirectNativeRequestContext::new(
        paths.canonical_project().to_path_buf(),
        paths.canonical_project().to_path_buf(),
        OsString::from("atlas"),
        vec![OsString::from("--opaque")],
        OsString::from("xterm-256color"),
        crate::native_bootstrap::NATIVE_WIRE_VERSION,
    )
    .with_environment([(OsString::from("PATH"), OsString::from("/attacker/bin"))]);
    let mut client = UnixStream::connect(paths.socket()).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut frame = crate::native_bootstrap::issue_request_bytes_for_tests(&context);
    frame.push(b'\n');
    client.write_all(&frame).unwrap();
    client
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":901,\"method\":\"native/bootstrap\",\"params\":{\"capability\":\"copied-secret-token\",\"env\":{\"PATH\":\"/attacker/bin\"}}}\n",
        )
        .unwrap();
    client.flush().unwrap();
    let mut response = String::new();
    let read = std::io::BufReader::new(client)
        .read_line(&mut response)
        .unwrap();
    if read > 0 {
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["id"], 901);
        assert_eq!(
            response["error"]["code"],
            marion_core::proto::error::METHOD_NOT_FOUND
        );
    }
    assert_eq!(native.capability_lookup_count(), 0);
    assert!(lock(&recorder.calls).is_empty());
    server.stop();
}

#[test]
fn native_accept_preparation_holds_the_cap_before_any_worker_spawn() {
    let native = NativeBootstrapService::disabled(std::path::PathBuf::from("/p"));
    let mut permits = Vec::new();
    while let Ok(permit) = native.try_acquire_connection() {
        permits.push(permit);
    }
    let spawned = AtomicU64::new(0);
    let (mut client, mut server) = UnixStream::pair().unwrap();
    if prepare_native_connection(&native, &mut server).is_some() {
        spawned.fetch_add(1, Ordering::SeqCst);
    }
    let mut status = [0u8];
    client.read_exact(&mut status).unwrap();
    assert_eq!(status, [1]);
    assert_eq!(spawned.load(Ordering::SeqCst), 0);
    assert_eq!(permits.len(), 32);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct SupportedNativeHandler(AtomicU64);

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl crate::native_bootstrap::NativeBootstrapHandler for SupportedNativeHandler {
    fn verify_terminal(
        &self,
        _peer: crate::native_bootstrap::PeerIdentity,
        _stdin: std::os::fd::BorrowedFd<'_>,
        _stdout: std::os::fd::BorrowedFd<'_>,
    ) -> Result<
        crate::native_bootstrap::TerminalGeometryObservation,
        crate::native_bootstrap::BootstrapError,
    > {
        Ok(crate::native_bootstrap::TerminalGeometryObservation::new(
            crate::native_bootstrap::TerminalGeometry {
                cols: 80,
                rows: 24,
                xpixel: 0,
                ypixel: 0,
            },
        ))
    }

    fn authorized(
        &self,
        _request: crate::native_bootstrap::ConsumedNativeRequest<'_>,
        _deadline: &crate::native_bootstrap::NativeLaunchDeadline,
    ) -> Result<
        crate::native_bootstrap::PendingNativeLaunchReceipt,
        crate::native_bootstrap::BootstrapError,
    > {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(crate::native_bootstrap::PendingNativeLaunchReceipt::new(
            crate::native_bootstrap::NativeLaunchReceipt::new(
                AgentId("019f0000-0000-7000-8000-000000000002".into()),
                crate::native_bootstrap::NativeLaunchTicket::for_test([0x31; 32]),
            ),
            || {},
        ))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn native_listener_dispatches_the_wire_and_native_connections_block_idle_until_cleanup() {
    use std::os::fd::AsFd;

    const GRACE: Duration = Duration::ZERO;
    let dir = marion_testsupport::scratch("native-dispatch");
    // A directory that exists: the service checks the request's working directory is one of
    // the project's before it hands the request on.
    let project = dir.join("p");
    std::fs::create_dir_all(&project).unwrap();
    let paths = crate::socket::socket_paths(&dir, &project, 1);
    let Acquired::Serving(serving) = acquire(&paths).unwrap() else {
        panic!("nothing was listening")
    };
    let leaver = Arc::new(Leaver::default());
    let handler = Arc::new(SupportedNativeHandler(AtomicU64::new(0)));
    let native = Arc::new(
        NativeBootstrapService::new_without_terminal_verification_for_task2_tests(
            paths.canonical_project().to_path_buf(),
            crate::native_bootstrap::NATIVE_WIRE_VERSION,
            Arc::clone(&handler) as Arc<dyn crate::native_bootstrap::NativeBootstrapHandler>,
        ),
    );
    let server = Server::start_with_native_handler(
        serving,
        Arc::clone(&leaver) as Arc<dyn Handle>,
        Arc::clone(&native),
        GRACE,
    );

    let idle_client = UnixStream::connect(paths.native_bootstrap()).unwrap();
    assert!(until(|| lock(&server.conns).len() == 1));
    let tick = leaver.ticks.load(Ordering::SeqCst);
    leaver.eligible.store(true, Ordering::SeqCst);
    assert!(until(|| leaver.ticks.load(Ordering::SeqCst) >= tick + 3));
    assert_eq!(leaver.asked.load(Ordering::SeqCst), 0);
    leaver.eligible.store(false, Ordering::SeqCst);
    drop(idle_client);
    assert!(until(|| lock(&server.conns).is_empty()));

    let mut client = UnixStream::connect(paths.native_bootstrap()).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let input = std::fs::File::open("/dev/null").unwrap();
    let output = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .unwrap();
    let context = crate::native_bootstrap::DirectNativeRequestContext::new(
        paths.canonical_project().to_path_buf(),
        paths.canonical_project().to_path_buf(),
        std::ffi::OsString::from("atlas"),
        Vec::new(),
        std::ffi::OsString::from("xterm"),
        crate::native_bootstrap::NATIVE_WIRE_VERSION,
    );
    let capability = crate::native_bootstrap::request_direct_cli_capability(
        &mut client,
        input.as_fd(),
        output.as_fd(),
        &context,
    )
    .unwrap();
    crate::native_bootstrap::present_direct_cli_capability(&mut client, capability, &context)
        .unwrap();
    assert_eq!(handler.0.load(Ordering::SeqCst), 1);
    server.stop();
    assert!(!paths.native_bootstrap().exists());
}

fn node_get(agent: &str) -> Call {
    Call::NodeGet(marion_core::proto::params::NodeGetParams::of(AgentId(
        agent.into(),
    )))
}

/// **§2's inbound notifications reach the handler and produce no frame.**
///
/// Both halves matter. A keystroke that never reached the handler is a dead keyboard; a
/// keystroke that produced a *response* would desynchronize every client, because a client
/// awaiting one request's answer would read the keystroke's answer instead — and it has no
/// `id` to correlate it by. The `node/get` after them is the proof of the second half: it gets
/// the next frame on the wire, so nothing was emitted in between.
#[test]
fn an_inbound_notification_reaches_the_handler_and_is_never_answered() {
    let f = Fixture::new("input");
    let mut s = f.dial();
    for input in [
        marion_core::proto::Input::NodePtyWrite {
            agent_id: AgentId("a".into()),
            bytes: "ls\r".into(),
        },
        marion_core::proto::Input::NodeResize {
            agent_id: AgentId("a".into()),
            cols: 140,
            rows: 40,
        },
    ] {
        let line = Frame::Input(marion_core::proto::ClientNotification::new(input)).to_line();
        s.write_all(line.as_bytes()).unwrap();
    }
    s.flush().unwrap();
    assert!(
        until(|| lock(&f.rec.inputs).len() == 2),
        "the handler never saw them"
    );
    let got: Vec<marion_core::proto::Input> =
        lock(&f.rec.inputs).iter().map(|(_, i)| i.clone()).collect();
    assert_eq!(
        got,
        vec![
            marion_core::proto::Input::NodePtyWrite {
                agent_id: AgentId("a".into()),
                bytes: "ls\r".into()
            },
            marion_core::proto::Input::NodeResize {
                agent_id: AgentId("a".into()),
                cols: 140,
                rows: 40
            },
        ]
    );

    let mut r = std::io::BufReader::new(s.try_clone().unwrap());
    send(&mut s, node_get("a"), 7);
    match read_frame(&mut r) {
        Frame::Response(resp) => assert_eq!(resp.id, RequestId::Number(7)),
        other => panic!("a keystroke put a frame on the wire: {other:?}"),
    }
}

/// A handler that panics on a keystroke must not take the operator's whole attach with it: the
/// connection reads on and the next request is still answered.
#[test]
fn a_panic_on_an_inbound_notification_does_not_end_the_connection() {
    let rec = Arc::new(Recorder::default());
    let handle = Arc::clone(&rec) as Arc<dyn Handle>;
    let out = sink(ConnId(1));
    let line = Frame::Input(marion_core::proto::ClientNotification::new(
        marion_core::proto::Input::NodePtyWrite {
            agent_id: AgentId("boom".into()),
            bytes: "x".into(),
        },
    ))
    .to_line();
    let mut stated = None;
    assert!(
        answer_one(ConnId(1), &line, &handle, &out, &mut stated).is_none(),
        "an inbound notification is never answered"
    );
}

#[test]
fn a_successful_quit_marks_its_connection_complete_only_after_building_the_response() {
    let rec = Arc::new(Recorder::default());
    rec.quit_ok.store(true, Ordering::SeqCst);
    let handle = Arc::clone(&rec) as Arc<dyn Handle>;
    let out = sink(ConnId(1));
    let request = Frame::Request(Request::new(
        RequestId::Number(1),
        Call::SessionQuit(marion_core::proto::params::SessionQuitParams {
            disposition: QuitDisposition::DetachAll,
        }),
    ));
    let mut stated = None;
    let (response, completed) = answer_one(
        ConnId(1),
        request.to_line().trim_end(),
        &handle,
        &out,
        &mut stated,
    )
    .expect("a request has a correlated response");
    assert!(
        completed,
        "only a successful quit closes after its response"
    );
    assert!(matches!(
        response.outcome,
        marion_core::proto::Outcome::Result(_)
    ));
    assert_eq!(stated, Some(QuitDisposition::DetachAll));
}

/// The request/response shape, end to end over the real socket: a call goes out, its answer
/// comes back on the same connection, correlated by `id`.
#[test]
fn a_call_is_answered_on_the_connection_that_made_it() {
    let fx = Fixture::new("rr");
    let mut c = fx.dial();
    send(&mut c, node_get("a"), 7);
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    let Frame::Response(resp) = read_frame(&mut r) else {
        panic!("expected a response")
    };
    assert_eq!(resp.id, RequestId::Number(7));
    let marion_core::proto::Outcome::Result(body) = resp.outcome else {
        panic!("expected a result")
    };
    assert_eq!(
        Method::NodeGet.decode_result(&body).unwrap(),
        MethodResult::NodeGet(NodeGetResult {
            node: a_node(),
            detail: Default::default()
        })
    );
}

/// **Two frames written as one `write` are two calls**, which is the same rule as the framing
/// unit tests, proven at the seam where a regression would actually happen: the supervisor's own
/// reader over a real socket.
#[test]
fn two_frames_in_one_write_are_both_answered_in_order() {
    let fx = Fixture::new("coalesced");
    let mut c = fx.dial();
    let a = Frame::Request(Request::new(RequestId::Number(1), node_get("a"))).to_line();
    let b = Frame::Request(Request::new(RequestId::Number(2), node_get("b"))).to_line();
    c.write_all(format!("{a}{b}").as_bytes()).unwrap();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    for want in [1, 2] {
        let Frame::Response(resp) = read_frame(&mut r) else {
            panic!("expected a response")
        };
        assert_eq!(resp.id, RequestId::Number(want));
    }
}

/// A frame delivered a byte at a time is still one call, and is answered once.
#[test]
fn a_call_dribbled_one_byte_at_a_time_is_answered_exactly_once() {
    let fx = Fixture::new("dribble");
    let mut c = fx.dial();
    let line = Frame::Request(Request::new(RequestId::Number(3), node_get("a"))).to_line();
    for b in line.as_bytes() {
        c.write_all(&[*b]).unwrap();
        c.flush().unwrap();
    }
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    let Frame::Response(resp) = read_frame(&mut r) else {
        panic!("expected a response")
    };
    assert_eq!(resp.id, RequestId::Number(3));
    assert!(until(|| lock(&fx.rec.calls).len() == 1), "answered once");
}

/// The subscription shape: after `tree/subscribe`, the supervisor can push a frame the client
/// never asked for, on the same connection.
#[test]
fn a_subscribed_connection_receives_a_frame_it_never_asked_for() {
    let fx = Fixture::new("sub");
    let mut c = fx.dial();
    send(
        &mut c,
        Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
        1,
    );
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    let Frame::Response(_) = read_frame(&mut r) else {
        panic!("the snapshot comes back as a response")
    };

    assert!(until(|| !lock(&fx.rec.subs).is_empty()));
    let out = lock(&fx.rec.subs)[0].clone();
    assert!(out.notify(Event::NodeState {
        agent_id: AgentId("a".into()),
        state: NodeState::Running,
        reap_state: ReapState::Live,
        ts: marion_core::encoding::SystemTime::from_unix_millis(1),
    }));
    let Frame::Notification(n) = read_frame(&mut r) else {
        panic!("a notification, not a response")
    };
    assert_eq!(n.event.method(), "node/state");
}

/// **NC — a client that closes without `session/quit` is provably distinguishable from one that
/// quit** (§7.3.1, §2).
///
/// Both connections end the same way from the socket's side: a close. The *only* difference is
/// that one stated a disposition first, and this asserts that marion's reading follows that
/// evidence and nothing else — including that the silent one yields `SocketClosed`, whose
/// `nodes_must_be_untouched` is the invariant §7.3.1 refuses to make conditional.
///
/// The `session/quit` handler here **refuses** the call (§7.3.2's dispositions are not built).
/// The distinction is recorded anyway, which is the point: the client said what it wanted
/// whether or not marion could do it, and a transport that only remembered *successful* quits
/// would have no way to tell these two apart the day the dispositions land.
#[test]
fn a_client_that_closes_without_session_quit_is_the_crash_case_not_a_quit() {
    let fx = Fixture::new("quit");

    // (1) states a disposition, then closes.
    let mut quitter = fx.dial();
    send(
        &mut quitter,
        Call::SessionQuit(marion_core::proto::params::SessionQuitParams {
            disposition: QuitDisposition::DetachAll,
        }),
        1,
    );
    let mut qr = std::io::BufReader::new(quitter.try_clone().unwrap());
    let Frame::Response(resp) = read_frame(&mut qr) else {
        panic!("expected a response")
    };
    let marion_core::proto::Outcome::Error(e) = resp.outcome else {
        panic!("§7.3.2's dispositions are not built, so this must be a refusal")
    };
    assert_eq!(
        e.kind(),
        Some(marion_core::proto::FailureKind::Unimplemented)
    );
    drop(qr);
    drop(quitter);

    // (2) says nothing at all, then closes — a SIGKILLed TUI, from the supervisor's side.
    let silent = fx.dial();
    drop(silent);

    assert!(until(|| lock(&fx.rec.gone).len() == 2), "both ended");
    let gone = lock(&fx.rec.gone).clone();
    let stated: Vec<&ClientGone> = gone.iter().map(|(_, g, _)| g).collect();
    assert!(
        stated.contains(&&ClientGone::Quit(QuitDisposition::DetachAll)),
        "the quitter's stated disposition survived to the departure: {stated:?}"
    );
    assert!(
        stated.contains(&&ClientGone::SocketClosed),
        "the silent client's close carries no disposition: {stated:?}"
    );
    // And the two are not merely different values — they differ on the predicate the invariant
    // is written in terms of.
    let quitter = stated
        .iter()
        .find(|g| matches!(g, ClientGone::Quit(_)))
        .unwrap();
    let silent = stated
        .iter()
        .find(|g| matches!(g, ClientGone::SocketClosed))
        .unwrap();
    assert!(silent.nodes_must_be_untouched(), "§7.3.1's invariant");
    assert!(!quitter.nodes_must_be_untouched());
    assert_eq!(silent.disposition(), None, "a close chose nothing");
    // Both saw the same thing at the transport, which is exactly why the protocol has to carry
    // the distinction: the socket cannot.
    for (_, _, why) in &gone {
        assert_eq!(*why, Departure::Eof, "identical from the socket's side");
    }
}

/// A successful detach ends **that client connection** with its result already delivered and
/// leaves the same transport/handler able to serve another client. This is disposition (b)'s
/// transport negative control: returning `Resident` in JSON is not proof if the connection
/// path commits the supervisor to exiting anyway. `socketpair` keeps the assertion at the
/// transport seam without making it depend on the host permitting a filesystem socket bind.
#[test]
fn a_successful_detach_closes_only_its_client_and_leaves_the_supervisor_serving() {
    let rec = Arc::new(Recorder::default());
    rec.quit_ok.store(true, Ordering::SeqCst);
    let handle = Arc::clone(&rec) as Arc<dyn Handle>;
    let stopping = Arc::new(AtomicBool::new(false));

    let (mut quitter, server_half) = UnixStream::pair().unwrap();
    let first = {
        let handle = Arc::clone(&handle);
        let stopping = Arc::clone(&stopping);
        std::thread::spawn(move || serve_conn(ConnId(1), server_half, handle, &stopping))
    };
    let mut qr = std::io::BufReader::new(quitter.try_clone().unwrap());
    send(
        &mut quitter,
        Call::SessionQuit(marion_core::proto::params::SessionQuitParams {
            disposition: QuitDisposition::DetachAll,
        }),
        1,
    );
    let Frame::Response(response) = read_frame(&mut qr) else {
        panic!("quit answers before closing")
    };
    assert!(matches!(
        response.outcome,
        marion_core::proto::Outcome::Result(_)
    ));
    let mut eof = String::new();
    assert_eq!(qr.read_line(&mut eof).unwrap(), 0, "successful quit closes");
    first.join().unwrap();
    assert!(matches!(
        &lock(&rec.gone)[0],
        (
            _,
            ClientGone::Quit(QuitDisposition::DetachAll),
            Departure::QuitCompleted
        )
    ));

    let (mut next, server_half) = UnixStream::pair().unwrap();
    let second = {
        let stopping = Arc::clone(&stopping);
        std::thread::spawn(move || serve_conn(ConnId(2), server_half, handle, &stopping))
    };
    let mut nr = std::io::BufReader::new(next.try_clone().unwrap());
    send(&mut next, node_get("a"), 2);
    let Frame::Response(response) = read_frame(&mut nr) else {
        panic!("detach ended the supervisor")
    };
    assert_eq!(response.id, RequestId::Number(2));
    drop(nr);
    drop(next);
    second.join().unwrap();
}

/// **NC — a client that stops reading is disconnected, not obeyed.**
///
/// §5.7 says a supervisor with a non-terminal node MUST keep running; a client that stops
/// reading must therefore not be able to apply backpressure into the supervisor. This subscribes
/// a client that never reads, pushes until the queue is judged full, and then proves the
/// supervisor is still serving by getting an answer on a **second** connection — which is the
/// half that matters, since a wedged supervisor would fail exactly there.
#[test]
fn a_client_that_stops_reading_is_dropped_rather_than_allowed_to_wedge_the_supervisor() {
    let fx = Fixture::new("slow");
    let mut deaf = fx.dial();
    send(
        &mut deaf,
        Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
        1,
    );
    assert!(until(|| !lock(&fx.rec.subs).is_empty()));
    let out = lock(&fx.rec.subs)[0].clone();

    // Push until the verdict lands. The cap is a safety net for a hang, not a measurement: the
    // assertion is that `send` says no, never that it says no by frame N.
    let mut pushed = 0usize;
    let mut refused = false;
    while pushed < 1_000_000 {
        let ok = out.notify(Event::NodeState {
            agent_id: AgentId("a".into()),
            state: NodeState::Running,
            reap_state: ReapState::Live,
            ts: marion_core::encoding::SystemTime::from_unix_millis(1),
        });
        pushed += 1;
        if !ok {
            refused = true;
            break;
        }
    }
    assert!(
        refused,
        "an unbounded queue is the same failure as no bound, with a slower onset"
    );
    assert!(
        matches!(out.departed(), Some(Departure::TooSlow { .. })),
        "the reason is named, not merely acted on: {:?}",
        out.departed()
    );

    // The supervisor is still serving, which is the property §5.7 actually requires.
    let mut healthy = fx.dial();
    send(&mut healthy, node_get("a"), 9);
    let mut r = std::io::BufReader::new(healthy.try_clone().unwrap());
    let Frame::Response(resp) = read_frame(&mut r) else {
        panic!("the supervisor stopped serving because one client stopped reading")
    };
    assert_eq!(resp.id, RequestId::Number(9));
    drop(deaf);
}

/// **NC — a handler that panics does not end the connection, let alone the supervisor.**
///
/// `watch.rs`'s rule with the stakes raised: this process holds every node's channel (§6.2), so
/// a client that can crash it by sending one request can end the fleet. The caller gets an
/// `Internal` — a malfunction, distinguishable from a refusal by code and by kind — and the very
/// next call on the same connection is answered normally.
#[test]
fn a_panicking_handler_answers_internal_and_the_connection_reads_on() {
    let fx = Fixture::new("panic");
    *lock(&fx.rec.panic_on) = Some(Method::TreeSubscribe);
    let mut c = fx.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());

    let hushed = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    send(
        &mut c,
        Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
        1,
    );
    let Frame::Response(resp) = read_frame(&mut r) else {
        panic!("expected a response")
    };
    std::panic::set_hook(hushed);

    let marion_core::proto::Outcome::Error(e) = resp.outcome else {
        panic!("a panic must not read as success")
    };
    assert_eq!(e.kind(), Some(marion_core::proto::FailureKind::Internal));
    assert!(!e.is_refusal(), "a malfunction is not a refusal");

    send(&mut c, node_get("a"), 2);
    let Frame::Response(resp) = read_frame(&mut r) else {
        panic!("the connection died of a handler's panic")
    };
    assert_eq!(resp.id, RequestId::Number(2));
}

/// A line marion cannot parse is answered **by its id** and costs exactly one line: NDJSON
/// re-synchronizes at the next newline, so degrading here means showing one error, not closing a
/// connection that is otherwise fine.
#[test]
fn a_malformed_line_is_answered_by_id_and_the_connection_survives_it() {
    let fx = Fixture::new("bad");
    let mut c = fx.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    c.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"node/frobnicate\",\"params\":{}}\n")
        .unwrap();
    let Frame::Response(resp) = read_frame(&mut r) else {
        panic!("expected a response")
    };
    assert_eq!(resp.id, RequestId::Number(5), "answered by its own id");
    let marion_core::proto::Outcome::Error(e) = resp.outcome else {
        panic!("expected an error")
    };
    assert_eq!(e.code, marion_core::proto::error::METHOD_NOT_FOUND);
    assert!(e.message.contains("node/frobnicate"), "{e}");

    c.write_all(b"this is not json at all\n").unwrap();
    // No id to answer with, so nothing comes back for that line — and the *next* call still
    // works, which is the property being asserted.
    send(&mut c, node_get("a"), 6);
    let Frame::Response(resp) = read_frame(&mut r) else {
        panic!("a bad line closed a connection it should only have cost one line")
    };
    assert_eq!(resp.id, RequestId::Number(6));
}

/// Two clients are two connections with two identities, which is what makes a per-connection
/// subscription possible at all.
#[test]
fn every_connection_has_its_own_identity() {
    let fx = Fixture::new("ids");
    let mut a = fx.dial();
    let mut b = fx.dial();
    send(&mut a, node_get("a"), 1);
    send(&mut b, node_get("b"), 1);
    let mut ra = std::io::BufReader::new(a.try_clone().unwrap());
    let mut rb = std::io::BufReader::new(b.try_clone().unwrap());
    read_frame(&mut ra);
    read_frame(&mut rb);
    let calls = lock(&fx.rec.calls).clone();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0].0, calls[1].0, "two clients, two ConnIds");
}
