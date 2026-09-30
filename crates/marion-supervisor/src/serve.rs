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

/// The back-off after an `accept` error that is neither "nothing pending" nor an interrupt: a
/// descriptor limit leaves the listener readable, and retrying at once would spin. Paid only on
/// that error, never while idle.
///
/// The accept loop does not otherwise sleep: it blocks in `poll(2)` on both listeners and a wake
/// pipe, with no timeout unless the handler names a [`Handle::next_deadline`] or §5.7's idle grace
/// is running. Every change the loop acts on wakes it — a connection arriving or leaving, a journal
/// fold, a handler state change ([`Handle::changes`]), [`Server::stop`] — so an idle supervisor
/// does not wake at all. The listeners stay non-blocking so a readable listener whose connection
/// vanished before `accept` cannot wedge the loop.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(5);

/// §5.7's proposed, explicitly unmeasured idle grace. Public because a supervisor launcher may
/// configure it; `Server::start` uses it rather than making zero the accidental default.
pub const DEFAULT_IDLE_GRACE: Duration = Duration::from_secs(300);

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
/// So it is only ever a second check behind the connection's `session/hello`, which is what says
/// whether a connection speaks for the operator or for one node (`handler::root_spawn_authorized`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peer {
    /// The kernel answered, and this is the peer's effective uid.
    Uid(u32),
    /// The kernel read failed, or this channel has no socket behind it. **Never treated as
    /// permission**: an unknown peer is refused wherever a known one would have been checked.
    Unknown,
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
/// returned `Peer::Uid(crate::socket::own_uid())` unconditionally would pass every test in this workspace while
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

    /// A native claim proved `conn` is node `agent`'s own terminal, before the connection's first
    /// frame: it speaks for that node, as a `session/hello` naming it would.
    fn claimed_by_native(&self, _conn: ConnId, _agent: &marion_core::AgentId) {}

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

    /// **The supervisor's pass**, called once per pass of the accept loop and once more after it
    /// ends. A handler with nothing to push does nothing here, which is the default.
    ///
    /// Notifications are the half of §2 that nothing else can drive. A [`Handle::call`] runs on its
    /// own connection's thread and can only answer *that* client; everything a supervisor says
    /// unprompted — §7.3.3's live leg, `tree/node-added`, `node/state` — is caused by a **file**
    /// changing, and no thread in this module is watching one. Before this existed, the only flush
    /// loop in the crate was a thread started by tests and by nothing in production, so a detached
    /// supervisor answered requests and never spoke first.
    ///
    /// **The accept loop rather than a new thread**, and that is a real choice. A pusher must be a
    /// *second* loop over the follower's result, never folded into the poll that keeps the tree
    /// current, because that one holds the registry lock and a client's socket must never get
    /// inside it. This is such a second loop. What it
    /// adds is that the accept loop is already a clock: it runs a pass whenever it is woken or its
    /// [`Self::next_deadline`] arrives, and it already asks the handle three questions per pass. A
    /// fourth thread would buy a cadence this one already has and cost a fourth thing to stop
    /// correctly.
    ///
    /// The bound on what a handler may do here is the bound already stated for
    /// [`Outbound::send`]: it never blocks, so a client that has stopped reading is a departure and
    /// not a stalled accept loop. A handler that would block on something else must not do it here.
    fn tick(&self) {}

    /// **When the accept loop must next call [`Self::tick`] even if nothing wakes it.** `None` —
    /// the default — is "not until something changes": a wake from [`Self::changes`], or a
    /// connection arriving or leaving. A handler whose [`Self::tick`] has work that is due at a
    /// *time* rather than on a change names that time here; one whose state changes without a
    /// wake must notify [`Self::changes`], because nothing else will run the loop.
    fn next_deadline(&self) -> Option<Instant> {
        None
    }

    /// The signal this handler notifies when something [`Self::tick`] or the idle predicates read
    /// has changed. The accept loop attaches its wake pipe to it.
    fn changes(&self) -> Option<Arc<crate::wake::Signal>> {
        None
    }

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
    /// The pid that connected, where the kernel says: what `session/hello` walks up to refuse the
    /// operator's key from inside a node's process tree.
    peer_pid: Option<u32>,
    tx: SyncSender<OutboundItem>,
    departed: Arc<Mutex<Option<Departure>>>,
    shutdown: Option<Arc<std::os::unix::net::UnixStream>>,
}

enum OutboundItem {
    Frame(Vec<u8>),
    Flow(OutboundFlow),
    /// Carries nothing: queued once, by the first departure, so a writer blocked on an empty queue
    /// wakes and sees the connection is over. The queue cannot close to say so — every subscription
    /// holding an [`Outbound`] clone keeps a sender alive — and polling for the departure cost each
    /// idle connection fifty wakeups a second.
    Wake,
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

    /// The pid that connected, where the kernel said. See [`Outbound::peer_pid`]'s field.
    pub fn peer_pid(&self) -> Option<u32> {
        self.peer_pid
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
        if slot.is_some() {
            return false;
        }
        *slot = Some(why);
        drop(slot);
        // A full queue needs no wake: the writer is busy, and checks the departure after every
        // item it takes.
        let _ = self.tx.try_send(OutboundItem::Wake);
        true
    }
}

fn prepare_outbound_item(
    out: &Outbound,
    item: OutboundItem,
) -> Option<(Vec<u8>, Option<OutboundFlow>)> {
    match item {
        OutboundItem::Wake => None,
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
        peer: Peer::Uid(crate::socket::own_uid()),
        peer_pid: None,
        tx,
        departed: Arc::new(Mutex::new(None)),
        shutdown: None,
    }
}

/// [`sink`], as though process `pid` had connected.
#[cfg(test)]
pub(crate) fn sink_from(conn: ConnId, pid: u32) -> Outbound {
    Outbound {
        peer_pid: Some(pid),
        ..sink(conn)
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
        peer: Peer::Uid(crate::socket::own_uid()),
        peer_pid: None,
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
    /// The accept loop's wake. [`Server::stop`] rings it so the loop does not wait out its poll.
    wake: Option<Arc<crate::wake::Pipe>>,
}

type Conns = Arc<Mutex<HashMap<ConnId, std::os::unix::net::UnixStream>>>;

/// **The most clients the main socket holds at once.** Each costs a reader and a writer thread, and
/// every process of the operator's uid can connect, so the count is bounded: one more is closed at
/// accept, before it costs anything. Far above a real fleet's use — a courier's connection lasts
/// one errand, and only a waiting parent or a watching screen holds one open.
pub const MAX_CLIENTS: usize = 256;

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
        Self::start_serving(serving, handle, native, idle_grace, MAX_CLIENTS)
    }

    /// [`Self::start`] with a client limit other than [`MAX_CLIENTS`], so a test can reach it.
    #[cfg(test)]
    fn start_with_client_limit(serving: Serving, handle: Arc<dyn Handle>, limit: usize) -> Server {
        let expected_project = serving.canonical_project().to_path_buf();
        Self::start_serving(
            serving,
            handle,
            Arc::new(NativeBootstrapService::disabled(expected_project)),
            DEFAULT_IDLE_GRACE,
            limit,
        )
    }

    fn start_serving(
        serving: Serving,
        handle: Arc<dyn Handle>,
        native: Arc<NativeBootstrapService>,
        idle_grace: Duration,
        max_clients: usize,
    ) -> Server {
        let stop = Arc::new(AtomicBool::new(false));
        let conns: Conns = Arc::new(Mutex::new(HashMap::new()));
        let wake = crate::wake::Pipe::new().ok().map(Arc::new);
        let accept = {
            let stop = Arc::clone(&stop);
            let conns = Arc::clone(&conns);
            let wake = wake.clone();
            std::thread::spawn(move || {
                accept_loop(
                    serving,
                    handle,
                    native,
                    stop,
                    conns,
                    idle_grace,
                    wake,
                    max_clients,
                )
            })
        };
        Server {
            stop,
            accept: Some(accept),
            conns,
            wake,
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
        if let Some(wake) = &self.wake {
            wake.wake();
        }
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

#[allow(clippy::too_many_arguments)]
fn accept_loop(
    serving: Serving,
    handle: Arc<dyn Handle>,
    native: Arc<NativeBootstrapService>,
    stop: Arc<AtomicBool>,
    conns: Conns,
    idle_grace: Duration,
    wake: Option<Arc<crate::wake::Pipe>>,
    max_clients: usize,
) {
    let next = AtomicU64::new(1);
    let mut idle_since: Option<Instant> = None;
    let mut threads: Vec<std::thread::JoinHandle<()>> = Vec::new();
    if let (Some(wake), Some(changes)) = (&wake, handle.changes()) {
        changes.attach(wake);
    }
    while !stop.load(Ordering::SeqCst) && !handle.exiting() {
        // **Drain, then look.** Anything that changed before a wake is seen by this pass; a wake
        // that lands during it leaves the pipe readable, so the wait below returns at once.
        if let Some(wake) = &wake {
            wake.drain();
        }
        handle.tick();
        if idle_grace_elapsed(&*handle, &conns, &mut idle_since, idle_grace)
            && handle.begin_idle_exit()
        {
            break;
        }
        accept_client(
            &serving,
            &handle,
            &stop,
            &conns,
            &next,
            &mut threads,
            &wake,
            max_clients,
        );
        if let Some(listener) = serving.native_bootstrap_listener() {
            accept_native(listener, &native, &conns, &next, &mut threads, &wake);
        }
        threads.retain(|t| !t.is_finished());
        if stop.load(Ordering::SeqCst) || handle.exiting() {
            break;
        }
        wait_for_work(
            &serving,
            wake.as_deref(),
            pass_deadline(&*handle, idle_since, idle_grace),
        );
    }
    // **One last tick after the flag, never before it** — `LiveRegistry::follow` is written to
    // this rule too, and for the same reason: what a node said
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

/// When the next pass must run with nothing waking it: the handler's own deadline or the end of a
/// running idle grace, whichever is first. `None` is neither: the loop sleeps until woken.
fn pass_deadline(
    handle: &dyn Handle,
    idle_since: Option<Instant>,
    idle_grace: Duration,
) -> Option<Instant> {
    let grace_ends = idle_since.and_then(|since| since.checked_add(idle_grace));
    match (handle.next_deadline(), grace_ends) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Block until a listener has a connection, the wake pipe rings, or `deadline` passes.
///
/// Without a wake pipe (no descriptor could be had) nothing would ring for a stop or a state
/// change, so the wait is capped at [`crate::wake::DEGRADED_RECHECK`].
fn wait_for_work(serving: &Serving, wake: Option<&crate::wake::Pipe>, deadline: Option<Instant>) {
    use std::os::fd::AsFd;
    let mut fds = vec![Some(serving.listener().as_fd()), wake.map(|w| w.fd())];
    if let Some(native) = serving.native_bootstrap_listener() {
        fds.push(Some(native.as_fd()));
    }
    crate::wake::wait_until(&fds, deadline);
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

/// One non-blocking pass over the client socket: admit a connection onto its own thread. Nothing
/// pending returns at once; the loop's `poll` is the wait.
#[allow(clippy::too_many_arguments)]
fn accept_client(
    serving: &Serving,
    handle: &Arc<dyn Handle>,
    stop: &Arc<AtomicBool>,
    conns: &Conns,
    next: &AtomicU64,
    threads: &mut Vec<std::thread::JoinHandle<()>>,
    wake: &Option<Arc<crate::wake::Pipe>>,
    max_clients: usize,
) {
    match serving.listener().accept() {
        // Dropped, which closes it: the client reads end-of-stream before its first answer.
        Ok(_) if lock(conns).len() >= max_clients => {}
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
            let wake = wake.clone();
            threads.push(std::thread::spawn(move || {
                serve_conn(id, stream, handle, &stopping);
                depart_conn(&conns, id, wake.as_deref());
            }));
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
        Err(_) => std::thread::sleep(ACCEPT_ERROR_BACKOFF),
    }
}

/// A connection's thread is done: forget its socket and wake the accept loop, since zero clients
/// is half of §5.7's idle predicate and the loop would otherwise learn of it only at its next
/// deadline.
fn depart_conn(conns: &Conns, id: ConnId, wake: Option<&crate::wake::Pipe>) {
    lock(conns).remove(&id);
    if let Some(wake) = wake {
        wake.wake();
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
    wake: &Option<Arc<crate::wake::Pipe>>,
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
            let wake = wake.clone();
            threads.push(std::thread::spawn(move || {
                native.serve_connection(id, stream, active);
                depart_conn(&conns, id, wake.as_deref());
            }));
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
        Err(_) => std::thread::sleep(ACCEPT_ERROR_BACKOFF),
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
            peer_pid: {
                use std::os::fd::AsRawFd;
                crate::native_bootstrap::peer_identity(stream.as_raw_fd())
                    .ok()
                    .map(|p| p.pid())
            },
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

/// Drain one connection's queue onto its socket until the connection is over.
///
/// **Blocks on the queue, and is woken by the departure.** The queue never closes while a
/// subscription holds an [`Outbound`] clone, so the end is the departure: [`Outbound::depart`] and
/// [`Outbound::fail`] queue an [`OutboundItem::Wake`] behind whatever is already waiting. Once the
/// connection has departed the writer finishes what is queued — a `session/quit` answer queued
/// before its own `QuitCompleted` departure still goes out — and returns when the queue is empty,
/// which is what the old 20 ms `recv_timeout` loop did, without its fifty idle wakeups a second.
/// Nothing is queued after a departure: `send`, `start_flow` and a flow's requeue all refuse once
/// it is recorded.
fn run_conn_writer(
    mut write_half: std::os::unix::net::UnixStream,
    rx: std::sync::mpsc::Receiver<OutboundItem>,
    out: Outbound,
    frame_write_timeout: Duration,
) {
    loop {
        let item = if out.departed().is_some() {
            match rx.try_recv() {
                Ok(item) => item,
                Err(_) => break,
            }
        } else {
            match rx.recv() {
                Ok(item) => item,
                Err(_) => break,
            }
        };
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
        // The inbound table, which is not an error: `node/pane-write`, `node/resize` and
        // `node/pane-ready`. Also unanswerable, and deliberately so — the acknowledgement of a
        // keystroke is the pty echo.
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
mod tests;
