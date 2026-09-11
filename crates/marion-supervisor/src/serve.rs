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
//! * **a panic in a handler is caught** and answered as [`marion_core::proto::FailureKind::Internal`]. A
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
//! [`marion_core::proto::ClientGone::SocketClosed`]. The recording is deliberately independent of what
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
#[cfg(test)]
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marion_core::proto::notify::Event;
use marion_core::proto::{
    Call, ClientGone, Frame, MethodResult, Notification, QuitDisposition, Request, RequestId,
    Response, RpcError,
};

use crate::native_bootstrap::{BootstrapError, NativeBootstrapService};
pub use crate::native_launch::NativeAdapterLookup;
use crate::socket::Serving;

/// What the enabled native bootstrap service is composed from.
///
/// The descriptor slice and the adapter lookup are data the composition root supplies: production
/// passes `PRODUCTION_NATIVE_FACADES` and its adapter table, an integration bed passes one
/// test-registered facade and a fixture adapter, and both go through the same constructor so the
/// production wiring is the wiring under test.
pub struct NativeLaunchConfig {
    pub descriptors: &'static [marion_core::NativeFacadeDescriptor],
    pub adapter_for: NativeAdapterLookup,
    /// The same spawn environment the handle owns; the native command factory needs the project
    /// directory, state root, bridge binary, endpoint, and auth mode to declare a node's bridge.
    pub env: crate::run::Env,
}

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

/// Maximum wall-clock time one serialized frame may occupy the connection writer.
pub(crate) const OUTBOUND_FRAME_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// One negotiated pane subscriber exceeded its bounded splice backlog.
    PaneOverflow { agent_id: String },
    /// The host-wide retained pane stream failed and can no longer support exact replay.
    PaneRetentionFailed { agent_id: String, error: String },
    /// An opaque pane input could not be durably evidenced or delivered. Notifications have no
    /// response envelope, so the exact negotiated connection closes rather than silently claiming
    /// that the terminal bytes were accepted.
    PaneInputFailed { agent_id: String, error: String },
    /// A client never completed the response-first pane handshake within its bounded lifetime.
    PaneReplayExpired { agent_id: String },
    /// A retained pane generation became unavailable after negotiation (replacement, failure, or
    /// completed-cache eviction). The connection ends rather than hanging without a terminal End.
    PaneReplayEvicted { agent_id: String },
    /// An internal paced producer panicked while the connection writer pulled its next frame.
    OutboundFlowPanicked,
    /// A `session/quit` completed and its response was queued, so marion closed this connection on
    /// purpose. Not `Eof`: the peer did not vanish, and calling it that would erase the voluntary
    /// half of §7.3 at the transport layer immediately after preserving it at the protocol layer.
    QuitCompleted,
    /// [`Server::stop`] ended it, not the client. Named so a shutdown is never mistaken for a
    /// fleet of clients crashing at once.
    ServerStopping,
}

/// **Who is on the other end of a connection, as the kernel reports it** — `getpeereid(2)` on
/// macOS, `getsockopt(SO_PEERCRED)` on Linux — read once at accept and never from anything the
/// peer said.
///
/// This is the *only* identity the transport can establish, and naming what it is not is the whole
/// point of the type. It answers **which user**. It does not answer **which node** — §5.4's
/// per-node capability token is what answers that, and [`marion_core::proto::SpawnCaller`] is where a
/// caller presents one. The two are not interchangeable and the design records a shipped instance
/// of confusing them (§11 item 28, open question 3): an unauthenticated local socket whose
/// authorization keyed on a client-asserted origin field, reachable by any process of the same
/// user. Peer credentials would not have saved that system, because the attacker was the same user.
///
/// So this is used for exactly one decision — whether a caller may create a **root**, which is a
/// spawn with no node behind it to prove anything about (see
/// `handler::root_spawn_authorized`) — and nothing else consults it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peer {
    /// The kernel answered, and this is the peer's effective uid.
    Uid(u32),
    /// The kernel read failed, or this channel has no socket behind it. **Never treated as
    /// permission**: an unknown peer is refused wherever a known one would have been checked.
    Unknown,
}

/// This process's own real uid — what a peer's uid is compared against.
pub fn own_uid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: reads the calling process's real uid and cannot fail.
    unsafe { getuid() }
}

/// [`Peer`] for one accepted connection.
///
/// Read from the socket rather than from the frame, and read **once**: a peer cannot change its uid
/// mid-connection, and re-asking per call would be a second answer that could differ from the one a
/// subscription was set up under.
fn peer_of(stream: &std::os::unix::net::UnixStream) -> Peer {
    use std::os::fd::AsRawFd;
    peer_of_fd(stream.as_raw_fd())
}

/// [`peer_of`] over a bare descriptor.
///
/// Split out **so the failure branch is reachable from a test**. Same uid on both ends of every
/// socket a `cargo test` can make, so the only half of this function a test can distinguish is the
/// one where the kernel says no — and a descriptor that is not a socket is exactly that answer,
/// for a real reason (`ENOTSOCK`). Without this seam, a build that ignored the failure and
/// returned `Peer::Uid(own_uid())` unconditionally would pass every test in this workspace while
/// granting root creation to a peer nobody had identified.
///
/// # Why there are two of these
///
/// The question — *what uid is on the other end of this socket* — is one question with two kernel
/// spellings, and neither is portable. `getpeereid(3)` is BSD and macOS; Linux answers the same
/// question through `getsockopt(SO_PEERCRED)`, which `native_bootstrap::peer_identity` already
/// reads through `rustix`. A single ungated `getpeereid` declaration linked on macOS and left
/// `undefined symbol: getpeereid` on Linux. Both arms fail closed to [`Peer::Unknown`], and the
/// test below is written over the *answer*, not over either syscall, so it pins both.
#[cfg(target_os = "macos")]
fn peer_of_fd(fd: i32) -> Peer {
    unsafe extern "C" {
        fn getpeereid(fd: i32, uid: *mut u32, gid: *mut u32) -> i32;
    }
    let (mut uid, mut gid) = (0u32, 0u32);
    // SAFETY: `fd` is open for the duration of this call — the only callers are `peer_of`, whose
    // stream owns it, and a test holding the file it opened — and both out-pointers are to live
    // locals. `getpeereid` writes them only on success.
    let rc = unsafe { getpeereid(fd, &mut uid, &mut gid) };
    if rc == 0 {
        Peer::Uid(uid)
    } else {
        Peer::Unknown
    }
}

/// [`peer_of_fd`] on Linux: `getsockopt(SO_PEERCRED)`, the same read
/// `native_bootstrap::peer_identity` makes, through the same `rustix` wrapper.
///
/// `SO_PEERCRED` carries a pid and a gid too; only the uid is taken, because [`Peer`] is the uid
/// and nothing else consults it. A descriptor that is not a socket fails here with `ENOTSOCK`
/// exactly as it does under `getpeereid`.
#[cfg(target_os = "linux")]
fn peer_of_fd(fd: i32) -> Peer {
    // SAFETY: `fd` is open for the duration of this call — the only callers are `peer_of`, whose
    // stream owns it, and a test holding the file it opened — and the borrow does not outlive it.
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    match rustix::net::sockopt::socket_peercred(borrowed) {
        Ok(credentials) => Peer::Uid(credentials.uid.as_raw()),
        Err(_) => Peer::Unknown,
    }
}

/// [`peer_of_fd`] where neither spelling has been measured: nobody is identified.
///
/// Not a guess and not a fall-through to this process's own uid — an unread peer is
/// [`Peer::Unknown`], which is refused wherever a known one would have been checked.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn peer_of_fd(_fd: i32) -> Peer {
    Peer::Unknown
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

    /// Deliver one **client→supervisor notification** (§2's inbound table): a keystroke or a
    /// resize for a node with a display plane.
    ///
    /// Returns nothing, and that is the whole shape of it. A notification has no `id`, so there is
    /// no frame in which an answer could be correlated — see [`marion_core::proto::input`] for why a
    /// keystroke must not be a request, and [`answer_one`]'s `None` for what the transport does
    /// with an id-less line either way. A handler that cannot deliver the bytes says so on the
    /// channel the operator is already watching: the node's own pty stream, or the refusal already
    /// given in `node/attach`'s answer.
    ///
    /// Defaulted to a no-op so a handler that implements no display plane does not have to write
    /// one. That is the same defaulting rule [`Handle::connected`] uses, and it is safe here for the
    /// same reason: a dropped keystroke on a node that has no pty is not a silent wrong answer, it
    /// is the only answer there is.
    fn input(&self, _conn: ConnId, _input: &marion_core::proto::Input) {}

    /// Deliver an inbound notification with the exact connection's outbound failure channel.
    ///
    /// This additive method preserves implementations of the original [`Self::input`] surface.
    /// Display-plane handlers override it when an id-less notification can fail only by visibly
    /// ending the sender's connection.
    fn input_with_out(&self, conn: ConnId, input: &marion_core::proto::Input, _out: &Outbound) {
        self.input(conn, input);
    }

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
    /// Who the kernel says is on the other end. Carried here rather than passed beside every call
    /// because it is a property of the connection and is read once, at accept — see [`Peer`].
    peer: Peer,
    tx: SyncSender<OutboundItem>,
    departed: Arc<Mutex<Option<Departure>>>,
    shutdown: Option<Arc<std::os::unix::net::UnixStream>>,
}

enum OutboundItem {
    Frame(Vec<u8>),
    Flow(OutboundFlow),
}

type OutboundFlow = Box<dyn FnMut() -> Option<Frame> + Send>;

enum FlowRequeue {
    Requeued,
    Dropped,
    Fatal,
}

trait FrameWriter: Write {
    fn set_frame_write_timeout(&mut self, timeout: Duration) -> std::io::Result<()>;
}

impl FrameWriter for std::os::unix::net::UnixStream {
    fn set_frame_write_timeout(&mut self, timeout: Duration) -> std::io::Result<()> {
        self.set_write_timeout(Some(timeout))
    }
}

fn write_frame_before<W, N>(
    writer: &mut W,
    frame: &[u8],
    timeout: Duration,
    now: N,
) -> std::io::Result<()>
where
    W: FrameWriter,
    N: Fn() -> Instant,
{
    let deadline = now().checked_add(timeout).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "outbound frame deadline overflowed",
        )
    })?;
    write_all_before(writer, frame, deadline, &now)?;
    flush_before(writer, deadline, &now)
}

/// What is left of the budget, or the expiry the caller reports instead of another syscall.
///
/// A socket timeout is per blocking syscall, so the budget has to be recomputed and reinstalled
/// before each one; an exhausted budget is an expiry rather than a zero timeout, which the kernel
/// would read as "block forever".
fn remaining_before(deadline: Instant, now: Instant) -> std::io::Result<Duration> {
    deadline
        .checked_duration_since(now)
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "outbound frame deadline expired",
            )
        })
}

/// Every byte of `frame`, under one deadline. A short write is the normal case on a socket and is
/// resumed; `Ok(0)` is not, because a writer that accepts nothing will accept nothing next time.
fn write_all_before<W, N>(
    writer: &mut W,
    frame: &[u8],
    deadline: Instant,
    now: &N,
) -> std::io::Result<()>
where
    W: FrameWriter,
    N: Fn() -> Instant,
{
    let mut unwritten = frame;
    while !unwritten.is_empty() {
        writer.set_frame_write_timeout(remaining_before(deadline, now())?)?;
        match writer.write(unwritten) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write the complete outbound frame",
                ));
            }
            Ok(written) => unwritten = &unwritten[written..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// The flush, under the same deadline: an interrupted flush is retried, anything else is the answer.
fn flush_before<W, N>(writer: &mut W, deadline: Instant, now: &N) -> std::io::Result<()>
where
    W: FrameWriter,
    N: Fn() -> Instant,
{
    loop {
        writer.set_frame_write_timeout(remaining_before(deadline, now())?)?;
        match writer.flush() {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

impl Outbound {
    pub fn conn(&self) -> ConnId {
        self.conn
    }

    /// The peer credentials this connection was accepted with. See [`Peer`] for what they answer
    /// and, more importantly, what they do not.
    pub fn peer(&self) -> Peer {
        self.peer
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
        match self
            .tx
            .try_send(OutboundItem::Frame(frame.to_line().into_bytes()))
        {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.fail(Departure::TooSlow {
                    queued: OUTBOUND_CAPACITY,
                });
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                // The writer is already gone and has recorded why; do not overwrite its reason with
                // a less specific one.
                self.fail(Departure::Eof);
                false
            }
        }
    }

    /// [`Self::send`], for the notification case that is most of this channel's traffic.
    pub fn notify(&self, event: Event) -> bool {
        self.send(&Frame::Notification(Notification::new(event)))
    }

    /// Schedule a writer-paced stream. The connection writer pulls one frame per queue turn and
    /// requeues the producer at the tail, bounding memory and preserving fairness with ordinary
    /// responses and notifications.
    pub(crate) fn start_flow(&self, flow: impl FnMut() -> Option<Frame> + Send + 'static) -> bool {
        if self.departed().is_some() {
            return false;
        }
        match self.tx.try_send(OutboundItem::Flow(Box::new(flow))) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.fail(Departure::TooSlow {
                    queued: OUTBOUND_CAPACITY,
                });
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                self.fail(Departure::Eof);
                false
            }
        }
    }

    pub fn departed(&self) -> Option<Departure> {
        lock(&self.departed).clone()
    }

    /// **First reason wins.** The writer thread's `WriteFailed` and the reader's `Eof` race on every
    /// ordinary disconnect, and the first one to notice saw the actual cause.
    fn depart(&self, why: Departure) {
        self.record_departure(why);
    }

    /// Record a connection-fatal producer failure and wake the socket reader so `gone` can remove
    /// every subscription. A departure flag alone cannot wake `Lines::next_line`.
    pub(crate) fn fail(&self, why: Departure) {
        if self.record_departure(why)
            && let Some(stream) = &self.shutdown
        {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    fn record_departure(&self, why: Departure) -> bool {
        let mut slot = lock(&self.departed);
        if slot.is_none() {
            *slot = Some(why);
            true
        } else {
            false
        }
    }
}

fn prepare_outbound_item(
    out: &Outbound,
    item: OutboundItem,
) -> Option<(Vec<u8>, Option<OutboundFlow>)> {
    match item {
        OutboundItem::Frame(frame) => Some((frame, None)),
        OutboundItem::Flow(_flow) if out.departed().is_some() => None,
        OutboundItem::Flow(mut flow) => {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut flow)) {
                Ok(frame) => frame.map(|frame| (frame.to_line().into_bytes(), Some(flow))),
                Err(_) => {
                    out.fail(Departure::OutboundFlowPanicked);
                    None
                }
            }
        }
    }
}

fn requeue_flow(out: &Outbound, flow: OutboundFlow) -> FlowRequeue {
    if out.departed().is_some() {
        return FlowRequeue::Dropped;
    }
    match out.tx.try_send(OutboundItem::Flow(flow)) {
        Ok(()) => FlowRequeue::Requeued,
        Err(TrySendError::Full(_)) => {
            out.fail(Departure::TooSlow {
                queued: OUTBOUND_CAPACITY,
            });
            FlowRequeue::Fatal
        }
        Err(TrySendError::Disconnected(_)) => {
            out.fail(Departure::Eof);
            FlowRequeue::Fatal
        }
    }
}

/// An [`Outbound`] with no socket behind it, for in-process callers.
///
/// The peer is **this process**, which is the literal truth rather than a convenience: there is no
/// second process on the other end of a channel whose receiver was dropped in this one. A
/// [`Peer::Unknown`] here would make every in-process caller fail the root-spawn check for a reason
/// that is not about authorization.
#[cfg(test)]
pub(crate) fn sink(conn: ConnId) -> Outbound {
    let (tx, _rx) = sync_channel(OUTBOUND_CAPACITY);
    Outbound {
        conn,
        peer: Peer::Uid(own_uid()),
        tx,
        departed: Arc::new(Mutex::new(None)),
        shutdown: None,
    }
}

/// A **live** in-process [`Outbound`] and the queue it fills.
///
/// [`sink`] drops its receiver, so every `send` on it fails at once — which is what its callers
/// want (a channel with nobody on it) and the opposite of what a test asserting *delivery* wants.
/// Holding the receiver is the whole difference: drop it and the `Outbound` behaves exactly like a
/// client whose process just died, which is how `pty.rs`'s M2 regression guard kills a client
/// without a second process.
#[cfg(test)]
pub(crate) fn capture(conn: ConnId) -> (Outbound, Captured) {
    let (tx, rx) = sync_channel(OUTBOUND_CAPACITY);
    let out = Outbound {
        conn,
        peer: Peer::Uid(own_uid()),
        tx,
        departed: Arc::new(Mutex::new(None)),
        shutdown: None,
    };
    (out.clone(), Captured { out, rx })
}

/// Test-side writer scheduler. It preserves the production queue's one-flow-frame-per-turn rule
/// without adding a background thread that could make ordering assertions depend on scheduling.
#[cfg(test)]
pub(crate) struct Captured {
    out: Outbound,
    rx: Receiver<OutboundItem>,
}

#[cfg(test)]
impl Captured {
    pub(crate) fn try_recv(&self) -> Result<Vec<u8>, TryRecvError> {
        loop {
            let Some((frame, flow)) = prepare_outbound_item(&self.out, self.rx.try_recv()?) else {
                continue;
            };
            if let Some(flow) = flow {
                let _ = requeue_flow(&self.out, flow);
            }
            return Ok(frame);
        }
    }

    pub(crate) fn try_iter(&self) -> CapturedTryIter<'_> {
        CapturedTryIter { captured: self }
    }
}

#[cfg(test)]
pub(crate) struct CapturedTryIter<'a> {
    captured: &'a Captured,
}

#[cfg(test)]
impl Iterator for CapturedTryIter<'_> {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        self.captured.try_recv().ok()
    }
}

/// Split a byte stream into `\n`-terminated frames.
///
/// **The buffer is the point.** See the module doc: a `read()` may return a fraction of a frame,
/// several frames, or several frames and a fraction, and the only thing that is always true is that
/// a frame ends at `\n` — which [`marion_core::proto::Frame::to_line`] guarantees is the *only* newline it
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
        let expected_project = serving.canonical_project().to_path_buf();
        Self::start_with_native_handler(
            serving,
            handle,
            Arc::new(NativeBootstrapService::disabled(expected_project)),
            idle_grace,
        )
    }

    /// Start with the **enabled** native bootstrap service: the detached supervisor's
    /// configuration, and the only path that composes authenticated selection, reservation, PTY
    /// launch, and claim onto the private native socket.
    ///
    /// Library callers that want a supervisor without a native lane keep
    /// [`Self::start_with_idle_grace`], whose service refuses every native request.
    pub fn start_with_native_launch(
        serving: Serving,
        handle: Arc<crate::handler::RegistryHandle>,
        native: NativeLaunchConfig,
        idle_grace: Duration,
    ) -> Result<Server, BootstrapError> {
        let expected_project = serving.canonical_project().to_path_buf();
        let factory = Arc::new(crate::native_launch::ProductionNativeCommandFactory::new(
            native.env,
            native.adapter_for,
        ));
        let mint_agent = Arc::new(|| {
            Ok(marion_core::new_agent_id(
                crate::clock::unix_millis(),
                crate::clock::entropy()?,
            ))
        });
        let handler = crate::native_launch::NativeLaunchHandler::new(
            native.descriptors,
            Arc::clone(&handle),
            Arc::new(crate::native_bootstrap::PendingNativeLaunches::new()),
            factory,
            mint_agent,
        )?;
        let service = NativeBootstrapService::new(
            expected_project,
            crate::native_bootstrap::NATIVE_WIRE_VERSION,
            Arc::new(handler),
        );
        Ok(Self::start_with_native_handler(
            serving,
            handle as Arc<dyn Handle>,
            Arc::new(service),
            idle_grace,
        ))
    }

    pub(crate) fn start_with_native_handler(
        serving: Serving,
        handle: Arc<dyn Handle>,
        native: Arc<NativeBootstrapService>,
        idle_grace: Duration,
    ) -> Server {
        let stop = Arc::new(AtomicBool::new(false));
        let conns: Conns = Arc::new(Mutex::new(HashMap::new()));
        let accept = {
            let stop = Arc::clone(&stop);
            let conns = Arc::clone(&conns);
            std::thread::spawn(move || {
                accept_loop(serving, handle, native, stop, conns, idle_grace)
            })
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
    native: Arc<NativeBootstrapService>,
    stop: Arc<AtomicBool>,
    conns: Conns,
    idle_grace: Duration,
) {
    let next = AtomicU64::new(1);
    let mut idle_since: Option<Instant> = None;
    let mut threads: Vec<std::thread::JoinHandle<()>> = Vec::new();
    while !stop.load(Ordering::SeqCst) && !handle.exiting() {
        handle.tick();
        if idle_grace_elapsed(&*handle, &conns, &mut idle_since, idle_grace)
            && handle.begin_idle_exit()
        {
            break;
        }
        accept_client(&serving, &handle, &stop, &conns, &next, &mut threads);
        if let Some(listener) = serving.native_bootstrap_listener() {
            accept_native(listener, &native, &conns, &next, &mut threads);
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

/// Whether the loop has sat with no clients and an idle-exit-eligible handle for the whole grace
/// (or the handle waived it). `idle_since` starts the first pass this holds and is cleared on any
/// pass it does not.
fn idle_grace_elapsed(
    handle: &dyn Handle,
    conns: &Conns,
    idle_since: &mut Option<Instant>,
    idle_grace: Duration,
) -> bool {
    let no_clients = lock(conns).is_empty();
    if !(no_clients && handle.idle_exit_eligible()) {
        *idle_since = None;
        return false;
    }
    let since = idle_since.get_or_insert_with(Instant::now);
    since.elapsed() >= idle_grace || handle.idle_exit_grace_waived()
}

/// One non-blocking pass over the client socket: admit a connection onto its own thread, or sleep
/// one poll interval when there is nothing to accept.
fn accept_client(
    serving: &Serving,
    handle: &Arc<dyn Handle>,
    stop: &Arc<AtomicBool>,
    conns: &Conns,
    next: &AtomicU64,
    threads: &mut Vec<std::thread::JoinHandle<()>>,
) {
    match serving.listener().accept() {
        Ok((stream, _)) => {
            let id = ConnId(next.fetch_add(1, Ordering::SeqCst));
            let _ = stream.set_nonblocking(false);
            if let Ok(dup) = stream.try_clone() {
                lock(conns).insert(id, dup);
            }
            handle.connected(id);
            let handle = Arc::clone(handle);
            let conns = Arc::clone(conns);
            let stopping = Arc::clone(stop);
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
}

/// One non-blocking pass over the native-bootstrap socket: admit an authenticated connection onto
/// its own thread. A connection the bootstrap refuses still consumed its `ConnId`.
fn accept_native(
    listener: &std::os::unix::net::UnixListener,
    native: &Arc<NativeBootstrapService>,
    conns: &Conns,
    next: &AtomicU64,
    threads: &mut Vec<std::thread::JoinHandle<()>>,
) {
    match listener.accept() {
        Ok((mut stream, _)) => {
            let id = ConnId(next.fetch_add(1, Ordering::SeqCst));
            let Some((dup, active)) = prepare_native_connection(native, &mut stream) else {
                return;
            };
            lock(conns).insert(id, dup);
            let native = Arc::clone(native);
            let conns = Arc::clone(conns);
            threads.push(std::thread::spawn(move || {
                native.serve_connection(id, stream, active);
                lock(&conns).remove(&id);
            }));
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
        Err(_) => std::thread::sleep(ACCEPT_POLL),
    }
}

fn prepare_native_connection(
    native: &NativeBootstrapService,
    stream: &mut std::os::unix::net::UnixStream,
) -> Option<(
    std::os::unix::net::UnixStream,
    crate::native_bootstrap::ActiveNativeConnection,
)> {
    stream.set_nonblocking(false).ok()?;
    let active = native.admit_connection(stream)?;
    let duplicate = stream.try_clone().ok()?;
    Some((duplicate, active))
}

/// One connection, start to finish.
fn serve_conn(
    id: ConnId,
    stream: std::os::unix::net::UnixStream,
    handle: Arc<dyn Handle>,
    stopping: &AtomicBool,
) {
    match PreparedClaimedConn::prepare(id, &stream, handle, OUTBOUND_FRAME_WRITE_TIMEOUT) {
        Ok(prepared) => {
            drop(stream);
            prepared.run_already_connected(stopping);
        }
        Err((handle, departure)) => {
            handle.gone(id, &ClientGone::SocketClosed, &departure);
        }
    }
}

/// Continue an authenticated native-bootstrap socket as an ordinary pane protocol connection.
///
/// The bootstrap accept loop already owns this `ConnId` and the server-wide socket duplicate.
/// Calling `connected` here, after the ticket was atomically claimed, makes the ensuing
/// `serve_conn`/`gone` pair the complete lifetime of the writer lease. Server shutdown closes the
/// duplicate and therefore still interrupts this otherwise blocking relay.
pub(crate) struct PreparedClaimedConn {
    id: ConnId,
    lines: Lines<std::os::unix::net::UnixStream>,
    handle: Arc<dyn Handle>,
    out: Option<Outbound>,
    writer: Option<std::thread::JoinHandle<()>>,
    writer_gate: Arc<(Mutex<PreparedConnGate>, std::sync::Condvar)>,
    departed: Arc<Mutex<Option<Departure>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreparedConnGate {
    Pending,
    Start,
    Abort,
}

impl PreparedClaimedConn {
    pub(crate) fn prepare(
        id: ConnId,
        stream: &std::os::unix::net::UnixStream,
        handle: Arc<dyn Handle>,
        frame_write_timeout: Duration,
    ) -> Result<Self, (Arc<dyn Handle>, Departure)> {
        Self::prepare_with_writer_spawner(
            id,
            stream,
            handle,
            frame_write_timeout,
            |name, task| std::thread::Builder::new().name(name).spawn(task),
            || {},
        )
    }

    #[cfg(test)]
    pub(crate) fn prepare_with_test_writer_spawner(
        id: ConnId,
        stream: &std::os::unix::net::UnixStream,
        handle: Arc<dyn Handle>,
        frame_write_timeout: Duration,
        spawn: impl FnOnce(
            String,
            Box<dyn FnOnce() + Send + 'static>,
        ) -> std::io::Result<std::thread::JoinHandle<()>>,
    ) -> Result<Self, (Arc<dyn Handle>, Departure)> {
        Self::prepare_with_writer_spawner(id, stream, handle, frame_write_timeout, spawn, || {})
    }

    #[cfg(test)]
    fn prepare_with_test_wait_hook(
        id: ConnId,
        stream: &std::os::unix::net::UnixStream,
        handle: Arc<dyn Handle>,
        frame_write_timeout: Duration,
        before_wait: impl FnOnce() + Send + 'static,
    ) -> Result<Self, (Arc<dyn Handle>, Departure)> {
        Self::prepare_with_writer_spawner(
            id,
            stream,
            handle,
            frame_write_timeout,
            |name, task| std::thread::Builder::new().name(name).spawn(task),
            before_wait,
        )
    }

    fn prepare_with_writer_spawner(
        id: ConnId,
        stream: &std::os::unix::net::UnixStream,
        handle: Arc<dyn Handle>,
        frame_write_timeout: Duration,
        spawn: impl FnOnce(
            String,
            Box<dyn FnOnce() + Send + 'static>,
        ) -> std::io::Result<std::thread::JoinHandle<()>>,
        before_wait: impl FnOnce() + Send + 'static,
    ) -> Result<Self, (Arc<dyn Handle>, Departure)> {
        let read_half = stream.try_clone().map_err(|_| {
            (
                Arc::clone(&handle),
                Departure::ReadFailed("the connection could not be split for reading".into()),
            )
        })?;
        let write_half = stream.try_clone().map_err(|_| {
            (
                Arc::clone(&handle),
                Departure::ReadFailed("the connection could not be split for writing".into()),
            )
        })?;
        let shutdown_half = stream.try_clone().map_err(|_| {
            (
                Arc::clone(&handle),
                Departure::ReadFailed(
                    "the connection could not retain a cancellation handle".into(),
                ),
            )
        })?;
        let departed = Arc::new(Mutex::new(None));
        let (tx, rx) = sync_channel::<OutboundItem>(OUTBOUND_CAPACITY);
        let out = Outbound {
            conn: id,
            peer: peer_of(stream),
            tx,
            departed: Arc::clone(&departed),
            shutdown: Some(Arc::new(shutdown_half)),
        };
        let writer_gate = Arc::new((
            Mutex::new(PreparedConnGate::Pending),
            std::sync::Condvar::new(),
        ));
        let worker_gate = Arc::clone(&writer_gate);
        let writer_out = out.clone();
        let mut before_wait = Some(before_wait);
        let writer = spawn(
            format!("marion-conn-writer-{}", id.0),
            Box::new(move || {
                let (gate, changed) = &*worker_gate;
                let mut state = gate.lock().unwrap_or_else(|error| error.into_inner());
                while *state == PreparedConnGate::Pending {
                    if let Some(before_wait) = before_wait.take() {
                        before_wait();
                    }
                    state = changed
                        .wait(state)
                        .unwrap_or_else(|error| error.into_inner());
                }
                let start = *state == PreparedConnGate::Start;
                drop(state);
                if !start {
                    return;
                }
                run_conn_writer(write_half, rx, writer_out, frame_write_timeout);
            }),
        )
        .map_err(|error| {
            (
                Arc::clone(&handle),
                Departure::ReadFailed(format!(
                    "the connection writer could not be started: {error}"
                )),
            )
        })?;
        Ok(Self {
            id,
            lines: Lines::new(read_half),
            handle,
            out: Some(out),
            writer: Some(writer),
            writer_gate,
            departed,
        })
    }

    /// Start a fully allocated connection. This is the post-ACK boundary and is infallible.
    pub(crate) fn run(mut self, stopping: &AtomicBool) {
        self.run_inner(stopping, true);
    }

    fn run_already_connected(mut self, stopping: &AtomicBool) {
        self.run_inner(stopping, false);
    }

    fn run_inner(&mut self, stopping: &AtomicBool, announce_connected: bool) {
        if announce_connected {
            self.handle.connected(self.id);
        }
        {
            let (gate, changed) = &*self.writer_gate;
            *gate.lock().unwrap_or_else(|error| error.into_inner()) = PreparedConnGate::Start;
            changed.notify_one();
        }
        let out = self.out.take().expect("prepared connection owns outbound");
        run_conn_reader(
            self.id,
            &mut self.lines,
            Arc::clone(&self.handle),
            stopping,
            out,
            self.writer.take().expect("prepared connection owns writer"),
            Arc::clone(&self.departed),
        );
    }
}

impl Drop for PreparedClaimedConn {
    fn drop(&mut self) {
        let out = self.out.take();
        if let Some(out) = out.as_ref() {
            out.depart(Departure::ReadFailed(
                "prepared claimed connection was aborted before acknowledgement".into(),
            ));
        }
        {
            let (gate, changed) = &*self.writer_gate;
            let mut state = gate.lock().unwrap_or_else(|error| error.into_inner());
            if *state == PreparedConnGate::Pending {
                *state = PreparedConnGate::Abort;
            }
            changed.notify_one();
        }
        drop(out);
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn run_conn_writer(
    mut write_half: std::os::unix::net::UnixStream,
    rx: std::sync::mpsc::Receiver<OutboundItem>,
    out: Outbound,
    frame_write_timeout: Duration,
) {
    loop {
        match rx.recv_timeout(WRITER_POLL) {
            Ok(item) => {
                let Some((frame, flow)) = prepare_outbound_item(&out, item) else {
                    continue;
                };
                if let Err(e) =
                    write_frame_before(&mut write_half, &frame, frame_write_timeout, Instant::now)
                {
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) {
                        out.fail(Departure::TooSlow {
                            queued: OUTBOUND_CAPACITY,
                        });
                    } else {
                        out.fail(Departure::WriteFailed(e.to_string()));
                    }
                    break;
                }
                if let Some(flow) = flow
                    && matches!(requeue_flow(&out, flow), FlowRequeue::Fatal)
                {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if out.departed().is_some() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the generic native connection runner is exercised before production routing"
    )
)]
fn serve_conn_with_frame_timeout(
    id: ConnId,
    stream: std::os::unix::net::UnixStream,
    handle: Arc<dyn Handle>,
    stopping: &AtomicBool,
    frame_write_timeout: Duration,
) {
    match PreparedClaimedConn::prepare(id, &stream, handle, frame_write_timeout) {
        Ok(prepared) => {
            drop(stream);
            prepared.run_already_connected(stopping);
        }
        Err((handle, departure)) => handle.gone(id, &ClientGone::SocketClosed, &departure),
    }
}

fn run_conn_reader(
    id: ConnId,
    lines: &mut Lines<std::os::unix::net::UnixStream>,
    handle: Arc<dyn Handle>,
    stopping: &AtomicBool,
    out: Outbound,
    writer: std::thread::JoinHandle<()>,
    departed: Arc<Mutex<Option<Departure>>>,
) {
    // §7.3.1's only evidence of intent. Set when a `session/quit` **parses**, independently of what
    // the handler answers — the client said what it wanted whether or not marion could do it.
    let mut stated: Option<QuitDisposition> = None;
    let departure = read_until_departure(id, lines, &handle, stopping, &out, &mut stated);
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

/// Read lines until something ends the connection, and say what that something was.
///
/// Every exit is a [`Departure`], because §7.3.1 needs a client's disappearance to be describable
/// rather than merely observed. The outbound half is re-checked after each line: a queue that
/// departed while the handler was running has already decided this connection is over, and reading
/// another line would only delay saying so.
fn read_until_departure(
    id: ConnId,
    lines: &mut Lines<std::os::unix::net::UnixStream>,
    handle: &Arc<dyn Handle>,
    stopping: &AtomicBool,
    out: &Outbound,
    stated: &mut Option<QuitDisposition>,
) -> Departure {
    loop {
        if stopping.load(Ordering::SeqCst) {
            return Departure::ServerStopping;
        }
        match lines.next_line() {
            Ok(None) => return Departure::Eof,
            Err(LineError::Oversize { bytes }) => return Departure::Oversize { bytes },
            Err(LineError::NotUtf8) => return Departure::NotUtf8,
            Err(LineError::Io(e)) => return Departure::ReadFailed(e),
            Ok(Some(line)) => {
                if let Some(departure) = answer_line(id, &line, handle, out, stated) {
                    return departure;
                }
            }
        }
        if let Some(d) = out.departed() {
            return d;
        }
    }
}

/// Answer one line and send what came back. `Some` only when the client's quit completed, which is
/// the one answer that also ends the connection.
fn answer_line(
    id: ConnId,
    line: &str,
    handle: &Arc<dyn Handle>,
    out: &Outbound,
    stated: &mut Option<QuitDisposition>,
) -> Option<Departure> {
    let (answer, quit_completed) = answer_one(id, line, handle, out, stated)?;
    out.send(&Frame::Response(answer));
    quit_completed.then_some(Departure::QuitCompleted)
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
            Some(answer_request(id, rid, call, handle, out, stated))
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
        // A *supervisor→client* notification arriving here is still a protocol error, and still an
        // unanswerable one: it carries no id. It is dropped rather than reported, exactly as before.
        Ok(Frame::Notification(n)) => {
            let _ = n;
            None
        }
        // The inbound table, which is not an error: §2's `node/pty-write` and `node/resize`. Also
        // unanswerable, and deliberately so — the acknowledgement of a keystroke is the pty echo.
        Ok(Frame::Input(n)) => {
            // Caught for the same reason `call` is: a handler that panics on a malformed keystroke
            // must not take the connection, and through it the operator's whole attach, with it.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle.input_with_out(id, &n.input, out)
            }));
            None
        }
        Err(e) => recover_id(line).map(|rid| (Response::err(rid, e), false)),
    }
}

/// Run one request and turn what came back into the response its sender can correlate.
///
/// The stated disposition is recorded **before** the handler runs, because §7.3.1 asks what the
/// client said it wanted, not what marion managed to do about it — a quit that panics was still a
/// quit. The handler is caught for the reason its own refusal text gives: a client that can end the
/// supervisor by sending a request is a client that can end the fleet (§5.7). Only a call that both
/// parsed as a quit and answered cleanly completes one.
fn answer_request(
    id: ConnId,
    rid: RequestId,
    call: Call,
    handle: &Arc<dyn Handle>,
    out: &Outbound,
    stated: &mut Option<QuitDisposition>,
) -> (Response, bool) {
    if let Call::SessionQuit(p) = &call {
        *stated = Some(p.disposition.clone());
    }
    let answered =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.call(id, &call, out)));
    let quit_completed = matches!(call, Call::SessionQuit(_)) && matches!(&answered, Ok(Ok(_)));
    let response = match answered {
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
    };
    (response, quit_completed)
}

/// The `id` of a frame marion could not otherwise parse.
///
/// A second, deliberately minimal parse: a malformed request still deserves an answer its sender can
/// correlate, and the alternative — silence — is the shape §11 item 23 keeps naming, where a caller
/// cannot tell a refusal from a request that never arrived. Only a string or a number counts, for
/// the reason [`marion_core::proto::RequestId`] gives: a `null` id cannot be correlated by anyone.
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
    /// which ignored the return code could not survive: `Peer::Uid(own_uid())` returned
    /// unconditionally would look right on every socket in this workspace while handing root
    /// creation to a peer nobody identified.
    #[test]
    fn a_peer_is_the_kernels_answer_and_an_unreadable_one_is_never_taken_as_permission() {
        use std::os::fd::AsRawFd;
        let (a, _b) = std::os::unix::net::UnixStream::pair().expect("a socket pair");
        assert_eq!(
            peer_of(&a),
            Peer::Uid(own_uid()),
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
        }
    }

    impl Handle for Recorder {
        fn connected(&self, conn: ConnId) {
            lock(&self.connected).push(conn);
        }

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
        // scheduler — the writer only notices a departure on its next `WRITER_POLL` tick, and under
        // load the wakeups behind that 20ms floor have outlasted a one-second bound, which read as
        // a leak the code had not committed.
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
        Call::NodeGet(marion_core::proto::params::NodeGetParams {
            agent_id: AgentId(agent.into()),
        })
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
        c.write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"node/frobnicate\",\"params\":{}}\n",
        )
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
}
