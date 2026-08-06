//! The supervisor's serve loop: NDJSON JSON-RPC 2.0 over the §2 socket.
//!
//! [`socket::acquire`](crate::socket::acquire) decides *who* serves. This decides what serving is:
//! accept connections, split the byte stream into frames, hand each call to a [`Handle`], and end a
//! connection in a way that says which of §7.3.1's two cases it was.
//!
//! # A `read()` is not a frame
//!
//! §11 item 1 (RESOLVED by S11) measured a pty capping reads at 1,024 bytes with **40% of them
//! carrying no frame boundary at all**, and states the rule this module obeys: *every consumer MUST
//! buffer and split, and MUST NOT treat a `read()` as a frame*. A unix socket is not a pty, but it
//! makes the same guarantee — none. [`Lines`] is therefore a buffering splitter with an explicit
//! cursor, and the property that a torn frame is delivered **once, when it completes** is a test
//! (`a_frame_split_across_reads_is_delivered_once_when_it_completes`), not a comment.
//!
//! It is the same discipline `journal.rs` and `registry.rs` already apply to the append-only
//! journal, and it is here for a sharper reason: a journal's torn tail is re-read from a file that
//! is not going anywhere, whereas a socket's torn tail exists only in this process's buffer. Drop it
//! and it is gone.
//!
//! # A slow client may not become the supervisor's problem
//!
//! §5.7: a supervisor with at least one non-terminal node **MUST keep running**. A client that stops
//! reading would, on a naive design, apply backpressure through `write` all the way into whatever
//! thread produced the notification — so one dead TUI could stall the fleet. Two things prevent it:
//!
//! * every connection has its **own writer thread** behind a **bounded** queue, so a blocked write
//!   blocks exactly one thread that does nothing else;
//! * [`Outbound::send`] never blocks. A full queue is not backpressure, it is a **verdict**: this
//!   client is not keeping up, and it is disconnected ([`Departure::TooSlow`]) rather than allowed to
//!   slow the supervisor down. §7.3.1 makes that safe — disconnecting a client does nothing to any
//!   node, by invariant.
//!
//! # The observer must never end the thing it observes
//!
//! `watch.rs`'s three rules, one layer up and with more at stake, because here the "observer" is the
//! process holding every node's channel (§6.2):
//!
//! * **degrade on bad input** — a line that is not a frame is answered, or recorded, and the
//!   connection reads on. NDJSON is self-synchronizing at `\n`, so one bad line costs one line;
//! * **a panic in a handler is caught** and answered as [`marion_proto::FailureKind::Internal`]. A
//!   client that can crash the supervisor by sending a request is a client that can kill the fleet;
//! * **announce your own death** — every connection ends through [`Handle::gone`] carrying both
//!   §7.3.1's reading *and* the transport-level [`Departure`], so a supervisor never simply stops
//!   hearing from someone without being able to say what it saw.
//!
//! # A dropped socket is not a quit
//!
//! §7.3.1 is absolute: *"a crashed, SIGKILLed, or otherwise vanished client MUST leave every node
//! exactly as it was"*, and *"guessing intent from a disconnect is how work gets killed by
//! accident."* The supervisor sees an identical close in both cases, so the **only** evidence of
//! intent that can ever exist is a `session/quit` that arrived first.
//!
//! This module records exactly that evidence and nothing else: a `session/quit` frame that *parses*
//! sets the connection's stated disposition, and every other departure yields
//! [`marion_proto::ClientGone::SocketClosed`]. The recording is deliberately independent of what
//! the handler answers — the client said what it wanted whether or not marion could do it. Only a
//! **successful** quit response closes that connection; a refused stale kill confirmation stays
//! open so the operator can render and retry. The handler performs dispositions during the call,
//! never from [`Handle::gone`], so EOF has no route to the default or to any other policy.
//!
//! # Exit is zero clients, then grace, then a record
//!
//! A handled quit makes exit eligible; it does not let the handler pretend the response socket has
//! already vanished. The accept loop owns the client count, resets its idle clock whenever any
//! client exists, and calls [`Handle::begin_idle_exit`] only after [`DEFAULT_IDLE_GRACE`] (or the
//! configured replacement). The callback journals the ordinary exit record before `exiting`
//! becomes true. Reversing those steps recreates the §5.7 ambiguity between *finished and left*
//! and *died*.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marion_proto::notify::Event;
use marion_proto::{
    Call, ClientGone, Frame, MethodResult, Notification, QuitDisposition, Request, RequestId,
    Response, RpcError,
};

use crate::socket::Serving;

/// The longest single frame the supervisor will assemble, in bytes.
///
/// A bound is not optional: without one, a peer that sends a megabyte a second and never a newline
/// grows this process's heap until the kernel picks a victim — and §5.7 says the one thing the
/// supervisor must not do is stop. 1 MiB is far above anything §2's vocabulary produces (the largest
/// realistic frame is a `tree/subscribe` snapshot of a whole forest) and far below a size that
/// threatens a supervisor.
///
/// Exceeding it is fatal **to the connection**, unlike a bad line: a line marion could not parse is
/// still a line, so the stream stays synchronized at the next `\n`, whereas an over-long frame means
/// marion does not know where the next frame begins.
pub const MAX_FRAME_BYTES: usize = 1 << 20;

/// How many frames may be queued for one client before it is judged not to be keeping up.
///
/// **A judgement, not a measurement** — no experiment in this repo bears on it. It is chosen to be
/// far more than a burst (a whole tree's `tree/node-added` storm is tens of frames) and far less
/// than an unbounded queue, which is the same failure as no bound at all with a slower onset.
pub const OUTBOUND_CAPACITY: usize = 1024;

/// How long the accept loop sleeps between polls when idle.
///
/// The listener is non-blocking so that [`Server::stop`] does not have to interrupt a blocking
/// `accept`, which on unix means either a self-connection or a signal — both of which are more
/// machinery than a sleep, and both of which can fail in ways a sleep cannot.
const ACCEPT_POLL: Duration = Duration::from_millis(5);

/// §5.7's proposed, explicitly unmeasured idle grace. Public because a supervisor launcher may
/// configure it; `Server::start` uses it rather than making zero the accidental default.
pub const DEFAULT_IDLE_GRACE: Duration = Duration::from_secs(300);

/// How long a connection's writer waits on its queue before re-checking whether the connection has
/// departed. See the writer loop for why it cannot simply wait for the channel to close.
const WRITER_POLL: Duration = Duration::from_millis(20);

/// A connection's identity, for the whole life of the supervisor. Monotonic and never reused, so a
/// log naming a connection names one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnId(pub u64);

/// **How a connection ended, at the transport.** Distinct from [`ClientGone`], which is §7.3.1's
/// *reading* of the same event, and the two are separate on purpose.
///
/// `ClientGone` answers "may marion touch the nodes?" and has exactly two answers, because §7.3.1
/// allows exactly two. This answers "what did marion see?", which is an operational question with as
/// many answers as there are ways for a socket to end. Folding them into one type would either give
/// the invariant more arms than it is allowed to have, or throw away the only information an
/// operator has about why their client vanished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Departure {
    /// The peer closed. The ordinary case, and the one §7.3.1 is about.
    Eof,
    /// The peer stopped reading and its queue filled. See the module doc: this is a verdict, not
    /// backpressure.
    TooSlow { queued: usize },
    /// A frame exceeded [`MAX_FRAME_BYTES`], so marion no longer knows where frames begin.
    Oversize { bytes: usize },
    /// A frame was not UTF-8. JSON is UTF-8 by definition, so this is not a JSON error — it is a
    /// stream marion cannot read as text at all.
    NotUtf8,
    /// The read failed for a reason that is neither of the above.
    ReadFailed(String),
    /// The write failed — the peer went away mid-answer, or the socket broke.
    WriteFailed(String),
    /// A `session/quit` completed and its response was queued, so marion closed this connection on
    /// purpose. Not `Eof`: the peer did not vanish, and calling it that would erase the voluntary
    /// half of §7.3 at the transport layer immediately after preserving it at the protocol layer.
    QuitCompleted,
    /// [`Server::stop`] ended it, not the client. Named so a shutdown is never mistaken for a
    /// fleet of clients crashing at once.
    ServerStopping,
}

/// What a supervisor does with a call.
///
/// One method per shape, and both take a [`ConnId`], because a subscription is per connection: the
/// same supervisor answers `tree/subscribe` on four sockets and must be able to tell them apart.
pub trait Handle: Send + Sync + 'static {
    /// A client was accepted. Separate from its first call: a silent connected client still counts
    /// against §5.7's absolute zero-client exit predicate.
    fn connected(&self, _conn: ConnId) {}

    /// Answer one call.
    ///
    /// `out` is this connection's notification channel. A handler that wants to *subscribe* the
    /// connection keeps a clone; a handler that only answers ignores it. Nothing a handler does with
    /// it can block — see [`Outbound::send`].
    ///
    /// A panic here is caught and answered as `Internal` rather than being allowed to end the
    /// connection or the supervisor.
    fn call(&self, conn: ConnId, call: &Call, out: &Outbound) -> Result<MethodResult, RpcError>;

    /// A connection ended. Both readings are supplied: §7.3.1's, which is what may act on nodes,
    /// and the transport's, which is what can be reported.
    fn gone(&self, conn: ConnId, gone: &ClientGone, why: &Departure);

    /// **The supervisor's heartbeat**, called once per pass of the accept loop and once more after
    /// it ends. A handler with nothing to push does nothing here, which is the default.
    ///
    /// Notifications are the half of §2 that nothing else can drive. A [`Handle::call`] runs on its
    /// own connection's thread and can only answer *that* client; everything a supervisor says
    /// unprompted — §7.3.3's live leg, `tree/node-added`, `node/state` — is caused by a **file**
    /// changing, and no thread in this module is watching one. Before this existed, the only flush
    /// loop in the crate (`handler::Broadcast`) was started by tests and by nothing in production,
    /// so a detached supervisor answered requests and never spoke first.
    ///
    /// **The accept loop rather than a new thread**, and that is a real choice. `Broadcast`'s doc
    /// gives the rule this obeys — a pusher must be a *second* loop over the follower's result,
    /// never folded into the poll that keeps the tree current, because that one holds the registry
    /// lock and a client's socket must never get inside it. This is such a second loop. What it
    /// adds is that the accept loop is already a clock: it is non-blocking, it wakes every
    /// [`ACCEPT_POLL`], and it already asks the handle three questions per pass. A fourth thread
    /// would buy a cadence this one already has and cost a fourth thing to stop correctly.
    ///
    /// The bound on what a handler may do here is the bound already stated for
    /// [`Outbound::send`]: it never blocks, so a client that has stopped reading is a departure and
    /// not a stalled accept loop. A handler that would block on something else must not do it here.
    fn tick(&self) {}

    /// Whether an explicit, fully handled `session/quit` has journaled the supervisor's exit.
    /// False by default so a handler that knows nothing about lifecycle can never acquire an
    /// implicit disposition merely by implementing transport.
    fn exiting(&self) -> bool {
        false
    }

    /// Every non-client clause of §5.7's exclusion list is clear. The client clause is this loop's.
    fn idle_exit_eligible(&self) -> bool {
        false
    }

    /// Whether a client **said** it was leaving, which is the only thing §5.7's grace is about.
    ///
    /// The grace is justified in §5.7 as a wait that *"should outlast an operator closing one
    /// window to open another"* — it bridges between clients that did not announce themselves. An
    /// explicit `session/quit` is that announcement, so waiting it out after one buys nothing and
    /// costs the operator a process they were told was going. A departure marion could not read —
    /// §7.3.1's crash — waits the whole grace, because for all marion knows a replacement window is
    /// already opening.
    ///
    /// False by default, so a handler that knows nothing about §7.3 can never shorten a wait it
    /// does not understand.
    fn idle_exit_grace_waived(&self) -> bool {
        false
    }

    /// Journal the exit and commit to stopping, after the server observed zero clients for its
    /// configured grace. False leaves the server resident; an exit record that could not be made
    /// durable must never be followed by the process disappearing cleanly.
    fn begin_idle_exit(&self) -> bool {
        false
    }
}

/// One connection's outbound half: a bounded queue drained by that connection's writer thread.
///
/// Cloneable, so a subscription can be held by whatever produces events without that producer ever
/// learning what a socket is.
#[derive(Debug, Clone)]
pub struct Outbound {
    conn: ConnId,
    tx: SyncSender<Vec<u8>>,
    departed: Arc<Mutex<Option<Departure>>>,
}

impl Outbound {
    pub fn conn(&self) -> ConnId {
        self.conn
    }

    /// Queue one frame. **Never blocks, and never fails silently.**
    ///
    /// `false` means this connection is finished — the queue was full (the client is not reading) or
    /// the writer is gone. The caller is not expected to retry; it is expected to drop its clone,
    /// which is how a subscriber list garbage-collects itself.
    pub fn send(&self, frame: &Frame) -> bool {
        if self.departed().is_some() {
            return false;
        }
        match self.tx.try_send(frame.to_line().into_bytes()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.depart(Departure::TooSlow {
                    queued: OUTBOUND_CAPACITY,
                });
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                // The writer is already gone and has recorded why; do not overwrite its reason with
                // a less specific one.
                self.depart(Departure::Eof);
                false
            }
        }
    }

    /// [`Self::send`], for the notification case that is most of this channel's traffic.
    pub fn notify(&self, event: Event) -> bool {
        self.send(&Frame::Notification(Notification::new(event)))
    }

    pub fn departed(&self) -> Option<Departure> {
        lock(&self.departed).clone()
    }

    /// **First reason wins.** The writer thread's `WriteFailed` and the reader's `Eof` race on every
    /// ordinary disconnect, and the first one to notice saw the actual cause.
    fn depart(&self, why: Departure) {
        let mut slot = lock(&self.departed);
        if slot.is_none() {
            *slot = Some(why);
        }
    }
}

#[cfg(test)]
pub(crate) fn sink(conn: ConnId) -> Outbound {
    let (tx, _rx) = sync_channel(OUTBOUND_CAPACITY);
    Outbound {
        conn,
        tx,
        departed: Arc::new(Mutex::new(None)),
    }
}

/// Split a byte stream into `\n`-terminated frames.
///
/// **The buffer is the point.** See the module doc: a `read()` may return a fraction of a frame,
/// several frames, or several frames and a fraction, and the only thing that is always true is that
/// a frame ends at `\n` — which [`marion_proto::Frame::to_line`] guarantees is the *only* newline it
/// ever emits.
#[derive(Debug)]
pub struct Lines<R> {
    src: R,
    buf: Vec<u8>,
    /// How far into `buf` the search for `\n` has already looked. Without it, a frame arriving one
    /// byte at a time costs O(n²) scanning — which is not hypothetical on a socket.
    scanned: usize,
    eof: bool,
}

/// Why a stream stopped yielding frames. Both variants end the connection; see [`Departure`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineError {
    Oversize { bytes: usize },
    NotUtf8,
    Io(String),
}

impl<R: Read> Lines<R> {
    pub fn new(src: R) -> Self {
        Self {
            src,
            buf: Vec::new(),
            scanned: 0,
            eof: false,
        }
    }

    /// The next complete frame, without its terminator. `Ok(None)` is end of stream.
    ///
    /// A partial frame at end of stream is **not** returned. It never completed, so treating it as a
    /// frame would mean acting on a request the client did not finish sending — which, for a
    /// vocabulary containing `node/kill`, is not a theoretical distinction. [`Self::pending`] is how
    /// its existence is still observable.
    pub fn next_line(&mut self) -> Result<Option<String>, LineError> {
        loop {
            if let Some(i) = self.buf[self.scanned..].iter().position(|b| *b == b'\n') {
                let end = self.scanned + i;
                let line: Vec<u8> = self.buf.drain(..=end).collect();
                self.scanned = 0;
                let line = &line[..line.len() - 1];
                return String::from_utf8(line.to_vec())
                    .map(Some)
                    .map_err(|_| LineError::NotUtf8);
            }
            self.scanned = self.buf.len();
            if self.buf.len() > MAX_FRAME_BYTES {
                return Err(LineError::Oversize {
                    bytes: self.buf.len(),
                });
            }
            if self.eof {
                return Ok(None);
            }
            let mut chunk = [0u8; 8192];
            match self.src.read(&mut chunk) {
                Ok(0) => self.eof = true,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(LineError::Io(e.to_string())),
            }
        }
    }

    /// Bytes of an unfinished frame currently buffered. Zero between frames.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

/// The running server: an accept loop, a connection per client, and the [`Serving`] that entitles
/// it to the socket at all.
///
/// It owns the `Serving` because the two lifetimes are the same one — a server that outlived its
/// lock could be serving a socket a second supervisor has already rebound.
pub struct Server {
    stop: Arc<AtomicBool>,
    accept: Option<std::thread::JoinHandle<()>>,
    conns: Conns,
}

type Conns = Arc<Mutex<HashMap<ConnId, std::os::unix::net::UnixStream>>>;

impl Server {
    /// Start accepting. Returns as soon as the loop is running; the loop outlives this call and is
    /// ended by [`Self::stop`] or by dropping the returned value.
    pub fn start(serving: Serving, handle: Arc<dyn Handle>) -> Server {
        Self::start_with_idle_grace(serving, handle, DEFAULT_IDLE_GRACE)
    }

    /// Start with an explicit §5.7 idle grace. This is the configuration seam; tests may choose
    /// zero to assert ordering without sleeping, while production's default remains 300 seconds.
    pub fn start_with_idle_grace(
        serving: Serving,
        handle: Arc<dyn Handle>,
        idle_grace: Duration,
    ) -> Server {
        let stop = Arc::new(AtomicBool::new(false));
        let conns: Conns = Arc::new(Mutex::new(HashMap::new()));
        let accept = {
            let stop = Arc::clone(&stop);
            let conns = Arc::clone(&conns);
            std::thread::spawn(move || accept_loop(serving, handle, stop, conns, idle_grace))
        };
        Server {
            stop,
            accept: Some(accept),
            conns,
        }
    }

    /// Stop accepting, end every open connection, and join.
    pub fn stop(mut self) {
        self.halt();
    }

    /// Block until the accept loop ends **on its own terms**, rather than ending it.
    ///
    /// This is the detached supervisor's main thread (§5.7): the only way out is the loop's own
    /// [`Handle::begin_idle_exit`], which journals the exit record *before* it breaks. A process
    /// that returns from here has therefore already written the record that lets a later reader tell
    /// *"it finished its work and left"* from *"it died"*, which is why the supervisor binary can
    /// simply exit 0 afterwards without any epitaph of its own.
    pub fn wait(mut self) {
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Shut every connection's read half down so its thread's blocking `read` returns rather than
        // waiting for a client that may never write again. §5.7 is about not exiting while work is
        // live, not about refusing to exit when asked.
        for (_, s) in lock(&self.conns).iter() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Server {
    /// A dropped handle must not leave an accept loop running against a socket the lock no longer
    /// covers. `halt` is idempotent, so this is a no-op after [`Server::stop`].
    fn drop(&mut self) {
        self.halt();
    }
}

fn accept_loop(
    serving: Serving,
    handle: Arc<dyn Handle>,
    stop: Arc<AtomicBool>,
    conns: Conns,
    idle_grace: Duration,
) {
    let next = AtomicU64::new(1);
    let mut idle_since: Option<Instant> = None;
    if serving.listener().set_nonblocking(true).is_err() {
        // Nothing else can be done from here and going quiet is the one forbidden option, so the
        // loop still runs: a blocking accept simply makes shutdown wait for a connection.
    }
    let mut threads: Vec<std::thread::JoinHandle<()>> = Vec::new();
    while !stop.load(Ordering::SeqCst) && !handle.exiting() {
        handle.tick();
        let no_clients = lock(&conns).is_empty();
        if no_clients && handle.idle_exit_eligible() {
            let since = idle_since.get_or_insert_with(Instant::now);
            let waited = since.elapsed() >= idle_grace || handle.idle_exit_grace_waived();
            if waited && handle.begin_idle_exit() {
                break;
            }
        } else {
            idle_since = None;
        }
        match serving.listener().accept() {
            Ok((stream, _)) => {
                let id = ConnId(next.fetch_add(1, Ordering::SeqCst));
                let _ = stream.set_nonblocking(false);
                if let Ok(dup) = stream.try_clone() {
                    lock(&conns).insert(id, dup);
                }
                handle.connected(id);
                let handle = Arc::clone(&handle);
                let conns = Arc::clone(&conns);
                let stopping = Arc::clone(&stop);
                threads.push(std::thread::spawn(move || {
                    serve_conn(id, stream, handle, &stopping);
                    lock(&conns).remove(&id);
                }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => std::thread::sleep(ACCEPT_POLL),
        }
        threads.retain(|t| !t.is_finished());
    }
    // **One last tick after the flag, never before it** — `LiveRegistry::follow` and
    // `handler::Broadcast` are both written to this rule and for the same reason: what a node said
    // in the moments before a shutdown is exactly what a watching client wanted, and a loop that
    // left on the flag without pushing would drop it and end the stream on a lie. The connections
    // are still open here; the shutdown below is what closes them.
    handle.tick();
    for (_, stream) in lock(&conns).iter() {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    for t in threads {
        let _ = t.join();
    }
}

/// One connection, start to finish.
fn serve_conn(
    id: ConnId,
    stream: std::os::unix::net::UnixStream,
    handle: Arc<dyn Handle>,
    stopping: &AtomicBool,
) {
    let Ok(write_half) = stream.try_clone() else {
        handle.gone(
            id,
            &ClientGone::SocketClosed,
            &Departure::ReadFailed("the connection could not be split for writing".into()),
        );
        return;
    };
    let departed = Arc::new(Mutex::new(None));
    let (tx, rx) = sync_channel::<Vec<u8>>(OUTBOUND_CAPACITY);
    let out = Outbound {
        conn: id,
        tx,
        departed: Arc::clone(&departed),
    };
    let writer = {
        let out = out.clone();
        std::thread::spawn(move || {
            let mut w = write_half;
            loop {
                match rx.recv_timeout(WRITER_POLL) {
                    Ok(frame) => {
                        if let Err(e) = w.write_all(&frame).and_then(|()| w.flush()) {
                            out.depart(Departure::WriteFailed(e.to_string()));
                            break;
                        }
                    }
                    // **The end of a connection is a departure, not a closed channel.** A
                    // subscriber list holding a clone of this `Outbound` keeps the sender alive
                    // indefinitely — which is the point of a clone — so waiting for the channel to
                    // close would leave one thread per dead client for the supervisor's whole life.
                    // `departed` is the authority on whether this connection is over, and
                    // [`Outbound::send`] already refuses on the strength of it, so a stale clone is
                    // inert rather than dangerous.
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if out.departed().is_some() {
                            break;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
    };

    // §7.3.1's only evidence of intent. Set when a `session/quit` **parses**, independently of what
    // the handler answers — the client said what it wanted whether or not marion could do it.
    let mut stated: Option<QuitDisposition> = None;
    let mut lines = Lines::new(stream);
    let departure = loop {
        if stopping.load(Ordering::SeqCst) {
            break Departure::ServerStopping;
        }
        match lines.next_line() {
            Ok(None) => break Departure::Eof,
            Err(LineError::Oversize { bytes }) => break Departure::Oversize { bytes },
            Err(LineError::NotUtf8) => break Departure::NotUtf8,
            Err(LineError::Io(e)) => break Departure::ReadFailed(e),
            Ok(Some(line)) => {
                if let Some((answer, quit_completed)) =
                    answer_one(id, &line, &handle, &out, &mut stated)
                {
                    out.send(&Frame::Response(answer));
                    if quit_completed {
                        break Departure::QuitCompleted;
                    }
                }
            }
        }
        if let Some(d) = out.departed() {
            break d;
        }
    };
    out.depart(departure);
    drop(out); // closes the queue, which ends the writer
    let _ = writer.join();
    let departure = lock(&departed).clone().unwrap_or(Departure::Eof);

    // The whole §7.3.1 decision, in one expression with no default arm. A `QuitDisposition` has no
    // `Default` precisely so that this cannot be written any other way.
    let gone = match stated {
        Some(d) => ClientGone::Quit(d),
        None => ClientGone::SocketClosed,
    };
    handle.gone(id, &gone, &departure);
}

/// Parse one line and produce the response, if a response is possible.
///
/// `None` means marion has nothing it can send: the line carried no `id`, so any answer would be
/// uncorrelatable — and §envelope refuses a `null` id exactly because *"the client's pending map
/// keeps waiting while the answer is discarded"*. The connection reads on regardless, because
/// NDJSON re-synchronizes at the next newline and one unreadable line is worth one unreadable line.
fn answer_one(
    id: ConnId,
    line: &str,
    handle: &Arc<dyn Handle>,
    out: &Outbound,
    stated: &mut Option<QuitDisposition>,
) -> Option<(Response, bool)> {
    match Frame::from_line(line) {
        Ok(Frame::Request(Request { id: rid, call, .. })) => {
            if let Call::SessionQuit(p) = &call {
                *stated = Some(p.disposition.clone());
            }
            let answered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle.call(id, &call, out)
            }));
            let quit_completed =
                matches!(call, Call::SessionQuit(_)) && matches!(&answered, Ok(Ok(_)));
            Some((
                match answered {
                    Ok(Ok(result)) => Response::ok(rid, &result),
                    Ok(Err(e)) => Response::err(rid, e),
                    Err(_) => Response::err(
                        rid,
                        RpcError::internal(
                            "the supervisor's handler panicked answering this call; the call did not \
                         complete and nothing about any node changed. The connection is still \
                         open, because a client that can end the supervisor by sending a request \
                         is a client that can end the fleet (§5.7).",
                        ),
                    ),
                },
                quit_completed,
            ))
        }
        // A client sending a *response* or a *notification* is a protocol error: §2's traffic is
        // client→supervisor requests and supervisor→client notifications, and nothing else. A
        // response at least carries an id, so the confusion can be named back to its sender.
        Ok(Frame::Response(r)) => Some((
            Response::err(
                r.id,
                RpcError::invalid_request(
                    "this is a response, and the supervisor sends no requests for a client to answer \
                 (§2). Nothing was done with it.",
                ),
            ),
            false,
        )),
        Ok(Frame::Notification(n)) => {
            let _ = n;
            None
        }
        Err(e) => recover_id(line).map(|rid| (Response::err(rid, e), false)),
    }
}

/// The `id` of a frame marion could not otherwise parse.
///
/// A second, deliberately minimal parse: a malformed request still deserves an answer its sender can
/// correlate, and the alternative — silence — is the shape §11 item 23 keeps naming, where a caller
/// cannot tell a refusal from a request that never arrived. Only a string or a number counts, for
/// the reason [`marion_proto::RequestId`] gives: a `null` id cannot be correlated by anyone.
fn recover_id(line: &str) -> Option<RequestId> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    match v.get("id")? {
        serde_json::Value::String(s) => Some(RequestId::Text(s.clone())),
        serde_json::Value::Number(n) => n.as_i64().map(RequestId::Number),
        _ => None,
    }
}

/// A poisoned lock is **taken**, never unwrapped — `registry.rs`'s rule, and for the same reason
/// §5.7 gives: a supervisor holding live nodes cannot afford to die of another thread's panic.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    use marion_proto::result::{NodeGetResult, TreeSubscribeResult};
    use marion_proto::{Method, NodeSummary, ReplayPoint};
    use std::io::BufRead;
    use std::os::unix::net::UnixStream;

    /// A `Handle` that records what it was asked and hands back a fixed answer.
    ///
    /// Deliberately not the registry: this file's subject is the transport, and a handler with real
    /// state would make every failure ambiguous between the two.
    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<(ConnId, Method)>>,
        gone: Mutex<Vec<(ConnId, ClientGone, Departure)>>,
        subs: Mutex<Vec<Outbound>>,
        panic_on: Mutex<Option<Method>>,
        quit_ok: AtomicBool,
    }

    fn a_node() -> NodeSummary {
        NodeSummary {
            agent_id: AgentId("a".into()),
            parent_id: None,
            name: None,
            agent_type: "codex-impl".into(),
            harness: Harness::Codex,
            depth: 0,
            state: NodeState::Idle,
            reap_state: ReapState::Live,
            timeout: EncDuration::from_secs(900),
        }
    }

    impl Handle for Recorder {
        fn call(
            &self,
            conn: ConnId,
            call: &Call,
            out: &Outbound,
        ) -> Result<MethodResult, RpcError> {
            lock(&self.calls).push((conn, call.method()));
            if *lock(&self.panic_on) == Some(call.method()) {
                panic!("a handler blew up");
            }
            match call {
                Call::NodeGet(_) => Ok(MethodResult::NodeGet(NodeGetResult { node: a_node() })),
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
                    MethodResult::SessionQuit(marion_proto::result::SessionQuitResult {
                        outcome: marion_proto::QuitOutcome::Detached {
                            detached: vec![AgentId("a".into())],
                            gate_exposed: vec![AgentId("a".into())],
                            guidance: marion_proto::DetachGuidance {
                                reattach: "call tree/subscribe".into(),
                                stop_fleet: "call session/quit KillTree".into(),
                            },
                            supervisor: marion_proto::SupervisorDisposition::Resident(
                                marion_proto::ResidentReason::NonTerminalNode,
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

        fn gone(&self, conn: ConnId, gone: &ClientGone, why: &Departure) {
            lock(&self.gone).push((conn, gone.clone(), why.clone()));
        }
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

    fn node_get(agent: &str) -> Call {
        Call::NodeGet(marion_proto::params::NodeGetParams {
            agent_id: AgentId(agent.into()),
        })
    }

    #[test]
    fn a_successful_quit_marks_its_connection_complete_only_after_building_the_response() {
        let rec = Arc::new(Recorder::default());
        rec.quit_ok.store(true, Ordering::SeqCst);
        let handle = Arc::clone(&rec) as Arc<dyn Handle>;
        let out = sink(ConnId(1));
        let request = Frame::Request(Request::new(
            RequestId::Number(1),
            Call::SessionQuit(marion_proto::params::SessionQuitParams {
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
        assert!(matches!(response.outcome, marion_proto::Outcome::Result(_)));
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
        let marion_proto::Outcome::Result(body) = resp.outcome else {
            panic!("expected a result")
        };
        assert_eq!(
            Method::NodeGet.decode_result(&body).unwrap(),
            MethodResult::NodeGet(NodeGetResult { node: a_node() })
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
            Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
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
            Call::SessionQuit(marion_proto::params::SessionQuitParams {
                disposition: QuitDisposition::DetachAll,
            }),
            1,
        );
        let mut qr = std::io::BufReader::new(quitter.try_clone().unwrap());
        let Frame::Response(resp) = read_frame(&mut qr) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Error(e) = resp.outcome else {
            panic!("§7.3.2's dispositions are not built, so this must be a refusal")
        };
        assert_eq!(e.kind(), Some(marion_proto::FailureKind::Unimplemented));
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
            Call::SessionQuit(marion_proto::params::SessionQuitParams {
                disposition: QuitDisposition::DetachAll,
            }),
            1,
        );
        let Frame::Response(response) = read_frame(&mut qr) else {
            panic!("quit answers before closing")
        };
        assert!(matches!(response.outcome, marion_proto::Outcome::Result(_)));
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
            Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
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
            Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
            1,
        );
        let Frame::Response(resp) = read_frame(&mut r) else {
            panic!("expected a response")
        };
        std::panic::set_hook(hushed);

        let marion_proto::Outcome::Error(e) = resp.outcome else {
            panic!("a panic must not read as success")
        };
        assert_eq!(e.kind(), Some(marion_proto::FailureKind::Internal));
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
        c.write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"node/frobnicate\",\"params\":{}}\n",
        )
        .unwrap();
        let Frame::Response(resp) = read_frame(&mut r) else {
            panic!("expected a response")
        };
        assert_eq!(resp.id, RequestId::Number(5), "answered by its own id");
        let marion_proto::Outcome::Error(e) = resp.outcome else {
            panic!("expected an error")
        };
        assert_eq!(e.code, marion_proto::error::METHOD_NOT_FOUND);
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
}
