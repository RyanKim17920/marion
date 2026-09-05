//! Native-only fd bootstrap and one-shot direct-CLI authorization.
//!
//! This module deliberately ends at authorization. It owns no vendor descriptor, process, PTY
//! master, artifact, or filesystem mutation. A successful consume produces only the sealed
//! provenance value that later layers may use.
//!
//! External crates can compute the request hash and use the client wire surface, but cannot mint
//! or consume server authority:
//!
//! ```compile_fail
//! use std::sync::Arc;
//! use marion_supervisor::native_bootstrap::{CapabilityAuthority, CapabilityRng, MonotonicClock};
//! let _server_authority = CapabilityAuthority::default();
//! ```
//!
//! The consumed proof is also invisible and has no external construction path:
//!
//! ```compile_fail
//! use marion_supervisor::native_bootstrap::ConsumedNativeCapability;
//! let _forged = ConsumedNativeCapability { selector: "atlas" };
//! ```
//!
//! Descriptor transport and token bytes are not public escape hatches:
//!
//! ```compile_fail
//! use std::os::fd::BorrowedFd;
//! use std::os::unix::net::UnixStream;
//! use marion_supervisor::native_bootstrap::{
//!     DirectNativeRequestContext, request_direct_cli_capability,
//! };
//! fn bypass(
//!     stream: &mut UnixStream,
//!     stdin: BorrowedFd<'_>,
//!     stdout: BorrowedFd<'_>,
//!     context: &DirectNativeRequestContext,
//! ) {
//!     let _ = request_direct_cli_capability(stream, stdin, stdout, context);
//! }
//! ```
//!
//! Capability presentation consumes the move-only value:
//!
//! ```compile_fail
//! use std::os::unix::net::UnixStream;
//! use marion_supervisor::native_bootstrap::{
//!     DirectCliCapability, DirectNativeRequestContext, present_direct_cli_capability,
//! };
//! fn replay(
//!     stream: &mut UnixStream,
//!     capability: DirectCliCapability,
//!     context: &DirectNativeRequestContext,
//! ) {
//!     let _ = present_direct_cli_capability(stream, capability, context);
//!     let _ = present_direct_cli_capability(stream, capability, context);
//! }
//! ```

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::IoSliceMut;
use std::io::{IoSlice, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rustix::fs::{OFlags, fcntl_getfl, fstat};
#[cfg(all(test, target_os = "macos"))]
use rustix::io::FdFlags;
#[cfg(target_os = "macos")]
use rustix::io::fcntl_dupfd_cloexec;
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
use rustix::io::fcntl_getfd;
#[cfg(target_os = "linux")]
use rustix::io::{FdFlags, fcntl_setfd};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg};
use rustix::net::{
    RecvFlags as SocketRecvFlags, SendAncillaryBuffer, SendAncillaryMessage, SendFlags, recv, send,
    sendmsg,
};

use marion_core::contract::AgentId;
use marion_core::ids::{UUID_LEN, is_uuid_v7};
use marion_core::production_native_facades;

use crate::native_intent::{
    AuthorizedNativeFacade, SelectedNativeFacade, select_consumed_direct_cli,
    select_consumed_native,
};
use crate::native_tty::{ClientTtyWitness, ControllingTtyWitness, verify_bootstrap_tty};
use crate::serve::{ConnId, own_uid};
use crate::socket;

const CONTEXT_DOMAIN: &[u8] = b"marion/direct-native-context/v1\0";
const DESCRIPTOR_MESSAGE: &[u8] = b"MNB1";
const CLAIM_MESSAGE: &[u8] = b"MNC1";
const CAPABILITY_TTL: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const LAUNCH_RESULT_TIMEOUT: Duration = Duration::from_secs(60);
const CLAIM_ACK_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_ACTIVE_NATIVE_CONNECTIONS: usize = 32;
const MAX_CONTEXT_FIELD_BYTES: usize = 64 * 1024;
const MAX_CONTEXT_ARGUMENTS: usize = 4096;
const MAX_CONTEXT_ENVIRONMENT_ENTRIES: usize = 4096;
const MAX_CONTEXT_BYTES: usize = 192 * 1024;
const MAX_SELECTOR_BYTES: usize = 255;
const MAX_CAPABILITY_GENERATION_ATTEMPTS: usize = 128;
pub(crate) const NATIVE_WIRE_VERSION: u32 = 1;

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NativeRouteTestStage {
    DescriptorsVerified,
    CapabilityConsumed,
    SelectorSelected,
}

#[cfg(test)]
thread_local! {
    static NATIVE_ROUTE_TEST_SENDER: std::cell::RefCell<
        Option<std::sync::mpsc::Sender<NativeRouteTestStage>>,
    > = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct NativeRouteTestHookGuard;

#[cfg(test)]
impl Drop for NativeRouteTestHookGuard {
    fn drop(&mut self) {
        NATIVE_ROUTE_TEST_SENDER.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

#[cfg(test)]
pub(crate) fn install_native_route_test_hook(
    sender: std::sync::mpsc::Sender<NativeRouteTestStage>,
) -> NativeRouteTestHookGuard {
    NATIVE_ROUTE_TEST_SENDER.with(|slot| {
        assert!(slot.borrow_mut().replace(sender).is_none());
    });
    NativeRouteTestHookGuard
}

#[cfg(test)]
pub(crate) fn observe_native_route_test_stage(stage: NativeRouteTestStage) {
    NATIVE_ROUTE_TEST_SENDER.with(|slot| {
        if let Some(sender) = slot.borrow().as_ref() {
            let _ = sender.send(stage);
        }
    });
}

/// The sole client for the native-only Task 2 transport.
#[derive(Debug)]
pub struct NativeBootstrapClient {
    stream: UnixStream,
    canonical_project: PathBuf,
}

impl NativeBootstrapClient {
    /// Resolve the current project and connect to its private native-bootstrap sibling socket.
    pub fn connect_for_cwd() -> Result<Self, BootstrapError> {
        let cwd = std::env::current_dir().map_err(BootstrapError::NativeTransportIo)?;
        let paths = socket::resolve(&cwd).map_err(|error| {
            BootstrapError::NativeTransportIo(std::io::Error::other(error.to_string()))
        })?;
        let stream = UnixStream::connect(paths.native_bootstrap())
            .map_err(BootstrapError::NativeTransportIo)?;
        Ok(Self {
            stream,
            canonical_project: paths.canonical_project().to_path_buf(),
        })
    }

    pub(crate) fn canonical_project(&self) -> &std::path::Path {
        &self.canonical_project
    }

    pub(crate) fn request(
        mut self,
        tty: ClientTtyWitness,
        context: DirectNativeRequestContext,
    ) -> Result<NativeBootstrapClientSession, BootstrapError> {
        let capability =
            request_direct_cli_capability(&mut self.stream, tty.stdin(), tty.stdout(), &context)?;
        Ok(NativeBootstrapClientSession {
            stream: self.stream,
            capability,
            context,
            tty,
        })
    }
}

/// One issued capability kept on the authenticated connection that issued it.
#[derive(Debug)]
pub(crate) struct NativeBootstrapClientSession {
    stream: UnixStream,
    capability: DirectCliCapability,
    context: DirectNativeRequestContext,
    tty: ClientTtyWitness,
}

impl NativeBootstrapClientSession {
    pub(crate) fn consume(mut self) -> Result<NativeFacadeHandoff, BootstrapError> {
        let receipt =
            present_direct_cli_capability(&mut self.stream, self.capability, &self.context)?;
        Ok(NativeFacadeHandoff {
            receipt,
            tty: self.tty,
            stream: self.stream,
        })
    }
}

/// The authenticated native launch result together with the caller's original terminal authority.
///
/// The terminal witness remains owned until the later transparent relay consumes this handoff. A
/// successful descriptor transfer therefore cannot accidentally discard the exact termios and
/// file-status baseline needed to restore the caller's terminal.
#[derive(Debug)]
pub struct NativeFacadeHandoff {
    receipt: NativeLaunchReceipt,
    tty: ClientTtyWitness,
    stream: UnixStream,
}

impl NativeFacadeHandoff {
    #[allow(dead_code, reason = "consumed by the later transparent relay slice")]
    pub(crate) fn into_parts(self) -> (NativeLaunchReceipt, ClientTtyWitness) {
        (self.receipt, self.tty)
    }

    pub(crate) fn claim(mut self) -> Result<ClaimedNativeFacade, BootstrapError> {
        exchange_native_claim(&mut self.stream, &self.receipt, |stream| {
            stream.set_read_timeout(None)?;
            stream.set_write_timeout(None)
        })?;
        let NativeLaunchReceipt {
            agent_id,
            ticket: _,
        } = self.receipt;
        Ok(ClaimedNativeFacade {
            agent_id,
            tty: self.tty,
            stream: self.stream,
        })
    }
}

fn exchange_native_claim(
    stream: &mut UnixStream,
    receipt: &NativeLaunchReceipt,
    reset_timeouts: impl FnOnce(&UnixStream) -> std::io::Result<()>,
) -> Result<(), BootstrapError> {
    stream
        .set_read_timeout(Some(LAUNCH_RESULT_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(LAUNCH_RESULT_TIMEOUT)))
        .map_err(BootstrapError::NativeTransportIo)?;
    reset_timeouts(stream).map_err(BootstrapError::NativeTransportIo)?;
    #[cfg(target_os = "macos")]
    rustix::net::sockopt::set_socket_nosigpipe(&*stream, true)
        .map_err(|error| BootstrapError::NativeTransportIo(error.into()))?;
    let deadline = Instant::now()
        .checked_add(LAUNCH_RESULT_TIMEOUT)
        .ok_or(BootstrapError::ClockOverflow)?;
    let claim = native_claim_request_bytes(receipt)?;
    send_native_claim_before(stream, &claim, deadline)?;
    let status = recv_native_claim_status_before(stream, deadline)?;
    if status != [WIRE_OK] {
        return Err(BootstrapError::AuthorizationRefused);
    }
    Ok(())
}

fn native_claim_request_bytes(receipt: &NativeLaunchReceipt) -> Result<Vec<u8>, BootstrapError> {
    let mut claim = Vec::with_capacity(CLAIM_MESSAGE.len() + 2 + UUID_LEN + 32);
    write_native_claim_request(&mut claim, receipt)?;
    Ok(claim)
}

fn wait_native_claim_io(
    stream: &UnixStream,
    flags: rustix::event::PollFlags,
    deadline: Instant,
) -> Result<(), BootstrapError> {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(BootstrapError::LaunchResultExpired)?;
        let timeout = rustix::event::Timespec {
            tv_sec: i64::try_from(remaining.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: remaining.subsec_nanos().into(),
        };
        let mut fds = [rustix::event::PollFd::new(stream, flags)];
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) => return Err(BootstrapError::LaunchResultExpired),
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(BootstrapError::NativeTransportIo(error.into())),
        }
    }
}

fn send_native_claim_before(
    stream: &UnixStream,
    claim: &[u8],
    deadline: Instant,
) -> Result<(), BootstrapError> {
    let mut unwritten = claim;
    while !unwritten.is_empty() {
        #[allow(
            unused_mut,
            reason = "Linux adds MSG_NOSIGNAL below; macOS uses SO_NOSIGPIPE"
        )]
        let mut flags = SendFlags::DONTWAIT;
        #[cfg(target_os = "linux")]
        {
            flags |= SendFlags::NOSIGNAL;
        }
        match send(stream, unwritten, flags) {
            Ok(0) => return Err(BootstrapError::NativeWireProtocol),
            Ok(written) => unwritten = &unwritten[written..],
            Err(rustix::io::Errno::INTR) => {}
            Err(error) if error == rustix::io::Errno::AGAIN => {
                wait_native_claim_io(stream, rustix::event::PollFlags::OUT, deadline)?;
            }
            Err(error) => return Err(BootstrapError::NativeTransportIo(error.into())),
        }
    }
    Ok(())
}

fn recv_native_claim_status_before(
    stream: &UnixStream,
    deadline: Instant,
) -> Result<[u8; 1], BootstrapError> {
    let mut status = [0xff];
    loop {
        match recv(stream, &mut status, SocketRecvFlags::DONTWAIT) {
            Ok((_, 0)) => return Err(BootstrapError::NativeWireProtocol),
            Ok((_, 1)) => return Ok(status),
            Ok(_) => return Err(BootstrapError::NativeWireProtocol),
            Err(rustix::io::Errno::INTR) => {}
            Err(error) if error == rustix::io::Errno::AGAIN => {
                wait_native_claim_io(stream, rustix::event::PollFlags::IN, deadline)?;
            }
            Err(error) => return Err(BootstrapError::NativeTransportIo(error.into())),
        }
    }
}

pub(crate) struct ClaimedNativeFacade {
    agent_id: AgentId,
    tty: ClientTtyWitness,
    stream: UnixStream,
}

impl ClaimedNativeFacade {
    pub(crate) fn into_parts(self) -> (AgentId, ClientTtyWitness, UnixStream) {
        (self.agent_id, self.tty, self.stream)
    }
}

#[cfg(target_os = "linux")]
const LINUX_ATOMIC_RECEIVE_FLAGS: RecvFlags = RecvFlags::CMSG_CLOEXEC;
#[cfg(target_os = "linux")]
const _: () = assert!(LINUX_ATOMIC_RECEIVE_FLAGS.contains(RecvFlags::CMSG_CLOEXEC));

/// The exact secret-free request state authenticated by the native bootstrap.
///
/// The client's process environment rides beside the hashed context: the vendor process must see
/// the operator's `PATH` and shell state, not the detached supervisor's, and the environment is
/// the only way that state can reach a process the supervisor spawns. It is carried, budgeted, and
/// stripped of `MARION_*` identity, but it is deliberately **not** a hash input (§5 runtime spec:
/// "Secrets and environment values are never hash inputs").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectNativeRequestContext {
    canonical_project: PathBuf,
    selector: OsString,
    opaque_tail: Vec<OsString>,
    terminal_profile: OsString,
    native_wire_version: u32,
    environment: Vec<(OsString, OsString)>,
}

impl DirectNativeRequestContext {
    pub fn new(
        canonical_project: PathBuf,
        selector: OsString,
        opaque_tail: Vec<OsString>,
        terminal_profile: OsString,
        native_wire_version: u32,
    ) -> Self {
        Self {
            canonical_project,
            selector,
            opaque_tail,
            terminal_profile,
            native_wire_version,
            environment: Vec::new(),
        }
    }

    /// Carry the client's environment to the authorized launch, minus every `MARION_*` name.
    ///
    /// Stripping happens here, on the client, so reserved identity never leaves the process; the
    /// server independently refuses a wire frame that still carries one.
    pub fn with_environment(
        mut self,
        environment: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Self {
        self.environment = environment
            .into_iter()
            .filter(|(name, _)| !is_reserved_marion_name(name))
            .collect();
        self
    }

    pub(crate) fn canonical_project(&self) -> &std::path::Path {
        &self.canonical_project
    }

    pub(crate) const fn native_wire_version(&self) -> u32 {
        self.native_wire_version
    }

    pub(crate) fn opaque_tail(&self) -> &[OsString] {
        &self.opaque_tail
    }

    pub(crate) fn terminal_profile(&self) -> &OsStr {
        &self.terminal_profile
    }

    pub(crate) fn environment(&self) -> &[(OsString, OsString)] {
        &self.environment
    }
}

fn is_reserved_marion_name(name: &OsStr) -> bool {
    name.as_bytes().starts_with(b"MARION_")
}

/// Domain-separated digest of a [`DirectNativeRequestContext`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContextHash([u8; 32]);

/// Hash raw Unix strings without UTF-8 conversion, while preserving every field and argument
/// boundary with explicit fixed-width framing.
pub fn context_hash(context: &DirectNativeRequestContext) -> ContextHash {
    context_hash_for_domain(CONTEXT_DOMAIN, context)
}

fn context_hash_for_domain(domain: &[u8], context: &DirectNativeRequestContext) -> ContextHash {
    fn bytes(hasher: &mut blake3::Hasher, value: &OsStr) {
        let value = value.as_bytes();
        hasher.update(&(value.len() as u64).to_be_bytes());
        hasher.update(value);
    }

    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    bytes(&mut hasher, context.canonical_project.as_os_str());
    bytes(&mut hasher, &context.selector);
    hasher.update(&(context.opaque_tail.len() as u64).to_be_bytes());
    for argument in &context.opaque_tail {
        bytes(&mut hasher, argument);
    }
    bytes(&mut hasher, &context.terminal_profile);
    hasher.update(&context.native_wire_version.to_be_bytes());
    ContextHash(*hasher.finalize().as_bytes())
}

/// Kernel-authenticated identity for the local bootstrap peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PeerIdentity {
    uid: u32,
    pid: u32,
}

impl PeerIdentity {
    #[cfg(test)]
    pub const fn uid(self) -> u32 {
        self.uid
    }

    pub(crate) const fn pid(self) -> u32 {
        self.pid
    }

    #[cfg(test)]
    pub(crate) fn current_for_tty_test() -> Self {
        Self {
            uid: own_uid(),
            pid: std::process::id(),
        }
    }

    /// A same-uid peer in **another** session, for verifier tests that must not run inside the
    /// client's own session the way the socket-pair fixtures do.
    #[cfg(test)]
    pub(crate) fn child_for_tty_test(pid: u32) -> Self {
        Self {
            uid: own_uid(),
            pid,
        }
    }
}

/// Initial terminal size verified by the supervisor from the received descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TerminalGeometry {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) xpixel: u16,
    pub(crate) ypixel: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TerminalFingerprint {
    device: u64,
    inode: u64,
    special_device: u64,
}

impl TerminalFingerprint {
    #[cfg(test)]
    pub(crate) const fn new(device: u64, inode: u64, special_device: u64) -> Self {
        Self {
            device,
            inode,
            special_device,
        }
    }
}

/// A received stdin/stdout pair bound to one identity and a later terminal observation.
#[derive(Debug)]
pub(crate) struct BoundTerminalDescriptors {
    connection: ConnId,
    peer: PeerIdentity,
    fingerprint: TerminalFingerprint,
    geometry: TerminalGeometry,
    witness: Option<ControllingTtyWitness>,
    _test_stdin: Option<OwnedFd>,
    _test_stdout: Option<OwnedFd>,
}

impl BoundTerminalDescriptors {
    fn revalidate_peer(&self) -> Result<(), BootstrapError> {
        if let Some(witness) = &self.witness {
            witness
                .revalidate_peer(self.peer)
                .map_err(|error| BootstrapError::TerminalVerification(error.to_string()))?;
        }
        Ok(())
    }

    pub(crate) fn into_controlling_tty_witness(self) -> Option<ControllingTtyWitness> {
        self.witness
    }
}

/// An authenticated connection on the native-only bootstrap transport.
#[derive(Debug)]
pub(crate) struct NativeBootstrapConnection {
    id: ConnId,
    peer: PeerIdentity,
    stream: UnixStream,
}

impl NativeBootstrapConnection {
    pub fn authenticate(id: ConnId, stream: UnixStream) -> Result<Self, BootstrapError> {
        let peer = peer_identity(stream.as_raw_fd())?;
        Self::authenticate_identity(id, stream, peer)
    }

    fn authenticate_identity(
        id: ConnId,
        stream: UnixStream,
        peer: PeerIdentity,
    ) -> Result<Self, BootstrapError> {
        if peer.uid != own_uid() {
            return Err(BootstrapError::PeerUidMismatch);
        }
        Ok(Self { id, peer, stream })
    }

    #[cfg(test)]
    const fn id(&self) -> ConnId {
        self.id
    }

    #[cfg(test)]
    const fn peer(&self) -> PeerIdentity {
        self.peer
    }

    /// Receive exactly the stdin/stdout pair and bind it to geometry from the later verified
    /// controlling-terminal witness. Task 3 owns terminal ioctls; this layer owns fd transport and
    /// identity only.
    #[cfg(test)]
    fn receive_terminal(
        &self,
        geometry: TerminalGeometry,
    ) -> Result<BoundTerminalDescriptors, BootstrapError> {
        let [stdin, stdout] = receive_terminal_descriptors(&self.stream)?;
        bind_terminal_identity(self.id, self.peer, geometry, stdin, stdout)
    }
}

/// Send exactly two descriptors on the native bootstrap stream using `SCM_RIGHTS`.
#[allow(
    dead_code,
    reason = "Task 3 exposes descriptor sending only through a verified ClientTtyWitness"
)]
fn send_terminal_descriptors(
    stream: &UnixStream,
    stdin: BorrowedFd<'_>,
    stdout: BorrowedFd<'_>,
) -> Result<(), BootstrapError> {
    let descriptors = [stdin, stdout];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !control.push(SendAncillaryMessage::ScmRights(&descriptors)) {
        return Err(BootstrapError::DescriptorMessage);
    }
    let written = sendmsg(
        stream,
        &[IoSlice::new(DESCRIPTOR_MESSAGE)],
        &mut control,
        SendFlags::empty(),
    )
    .map_err(io_error)?;
    if written != DESCRIPTOR_MESSAGE.len() {
        return Err(BootstrapError::DescriptorMessage);
    }
    Ok(())
}

#[cfg(test)]
fn receive_terminal_descriptors(stream: &UnixStream) -> Result<[OwnedFd; 2], BootstrapError> {
    let deadline = HandshakeDeadline::new(Arc::new(SystemClock::default()), HANDSHAKE_TIMEOUT)?;
    receive_terminal_descriptors_before(stream, &deadline)
}

fn receive_terminal_descriptors_before(
    stream: &UnixStream,
    deadline: &HandshakeDeadline,
) -> Result<[OwnedFd; 2], BootstrapError> {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (stream, deadline);
        Err(BootstrapError::AtomicCloexecUnsupported)
    }

    #[cfg(target_os = "linux")]
    {
        receive_terminal_descriptors_atomic(stream, deadline)
    }

    #[cfg(target_os = "macos")]
    {
        receive_terminal_descriptors_non_atomic(stream, deadline)
    }
}

#[cfg(target_os = "linux")]
fn receive_terminal_descriptors_atomic(
    stream: &UnixStream,
    deadline: &HandshakeDeadline,
) -> Result<[OwnedFd; 2], BootstrapError> {
    deadline.install_read_timeout(stream)?;
    let mut message = [0u8; DESCRIPTOR_MESSAGE.len()];
    let mut iov = [IoSliceMut::new(&mut message)];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4)) - 1];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let received =
        recvmsg(stream, &mut iov, &mut control, LINUX_ATOMIC_RECEIVE_FLAGS).map_err(io_error)?;
    let mut descriptors = Vec::new();
    for ancillary in control.drain() {
        if let RecvAncillaryMessage::ScmRights(rights) = ancillary {
            descriptors.extend(rights);
        }
    }
    #[cfg(test)]
    post_recv_test_hook(&descriptors);
    if received.flags.contains(rustix::net::ReturnFlags::CTRUNC) || descriptors.len() > 3 {
        return Err(BootstrapError::AncillaryTruncated);
    }
    if received.bytes == 0 || message[..received.bytes] != DESCRIPTOR_MESSAGE[..received.bytes] {
        return Err(BootstrapError::DescriptorMessage);
    }
    let [stdin, stdout]: [OwnedFd; 2] = descriptors
        .try_into()
        .map_err(|_| BootstrapError::DescriptorCount)?;
    fcntl_setfd(&stdin, FdFlags::CLOEXEC).map_err(io_error)?;
    fcntl_setfd(&stdout, FdFlags::CLOEXEC).map_err(io_error)?;
    if received.bytes < DESCRIPTOR_MESSAGE.len() {
        let mut transport = DeadlineIo::new(stream, deadline);
        read_descriptor_message_tail(&mut transport, &mut message, received.bytes)?;
        if message != DESCRIPTOR_MESSAGE {
            return Err(BootstrapError::DescriptorMessage);
        }
    }
    Ok([stdin, stdout])
}

#[cfg(target_os = "macos")]
fn receive_terminal_descriptors_non_atomic(
    stream: &UnixStream,
    deadline: &HandshakeDeadline,
) -> Result<[OwnedFd; 2], BootstrapError> {
    deadline.install_read_timeout(stream)?;
    let mut message = [0u8; DESCRIPTOR_MESSAGE.len()];
    let received = {
        let receive = || {
            crate::spawn_receive_gate::SPAWN_RECEIVE_GATE.receive_non_atomic(|| {
                let mut iov = [IoSliceMut::new(&mut message)];
                let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4)) - 1];
                let mut control = RecvAncillaryBuffer::new(&mut space);
                deadline.install_read_timeout(stream)?;
                let received = recvmsg(stream, &mut iov, &mut control, RecvFlags::empty())
                    .map_err(io_error)?;
                #[cfg(test)]
                pre_drain_unwind_test_hook();
                let mut originals = Vec::new();
                for ancillary in control.drain() {
                    if let RecvAncillaryMessage::ScmRights(rights) = ancillary {
                        originals.extend(rights);
                    }
                }
                #[cfg(test)]
                post_recv_test_hook(&originals);
                if received.flags.contains(rustix::net::ReturnFlags::CTRUNC) || originals.len() > 3
                {
                    return Err(BootstrapError::AncillaryTruncated);
                }
                if received.bytes == 0
                    || iov[0][..received.bytes] != DESCRIPTOR_MESSAGE[..received.bytes]
                {
                    return Err(BootstrapError::DescriptorMessage);
                }
                if originals.len() != 2 {
                    return Err(BootstrapError::DescriptorCount);
                }
                let mut duplicates = Vec::with_capacity(2);
                for original in &originals {
                    duplicates.push(fcntl_dupfd_cloexec(original, 3).map_err(io_error)?);
                }
                drop(originals);
                let descriptors = duplicates
                    .try_into()
                    .map_err(|_| BootstrapError::DescriptorCount)?;
                Ok((descriptors, received.bytes))
            })
        };
        let received = observe_receive_unwind(receive);
        received?
    };
    let (descriptors, received_bytes) = received;
    if received_bytes < DESCRIPTOR_MESSAGE.len() {
        let mut transport = DeadlineIo::new(stream, deadline);
        read_descriptor_message_tail(&mut transport, &mut message, received_bytes)?;
        if message != DESCRIPTOR_MESSAGE {
            return Err(BootstrapError::DescriptorMessage);
        }
    }
    Ok(descriptors)
}

#[cfg(all(test, target_os = "macos"))]
struct ReceiveUnwindTestEndpoint {
    received: std::sync::mpsc::SyncSender<()>,
    panic_now: std::sync::mpsc::Receiver<()>,
    unwound: std::sync::mpsc::SyncSender<()>,
    resume_unwind: std::sync::mpsc::Receiver<()>,
}

#[cfg(all(test, target_os = "macos"))]
thread_local! {
    static RECEIVE_UNWIND_TEST_HOOK: std::cell::RefCell<Option<ReceiveUnwindTestEndpoint>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(all(test, target_os = "macos"))]
struct ReceiveUnwindTestControl {
    received: std::sync::mpsc::Receiver<()>,
    panic_now: std::sync::mpsc::SyncSender<()>,
    unwound: std::sync::mpsc::Receiver<()>,
    resume_unwind: std::sync::mpsc::SyncSender<()>,
}

#[cfg(all(test, target_os = "macos"))]
struct ReceiveUnwindTestInstaller(Option<ReceiveUnwindTestEndpoint>);

#[cfg(all(test, target_os = "macos"))]
impl ReceiveUnwindTestInstaller {
    fn install(mut self) -> InstalledReceiveUnwindTestHook {
        let endpoint = self.0.take().expect("receive unwind hook installed once");
        RECEIVE_UNWIND_TEST_HOOK.with(|hook| {
            assert!(hook.borrow_mut().replace(endpoint).is_none());
        });
        InstalledReceiveUnwindTestHook
    }
}

#[cfg(all(test, target_os = "macos"))]
struct InstalledReceiveUnwindTestHook;

#[cfg(all(test, target_os = "macos"))]
impl Drop for InstalledReceiveUnwindTestHook {
    fn drop(&mut self) {
        RECEIVE_UNWIND_TEST_HOOK.with(|hook| {
            hook.borrow_mut().take();
        });
    }
}

#[cfg(all(test, target_os = "macos"))]
fn receive_unwind_test_hook() -> (ReceiveUnwindTestControl, ReceiveUnwindTestInstaller) {
    let (received_tx, received_rx) = std::sync::mpsc::sync_channel(0);
    let (panic_tx, panic_rx) = std::sync::mpsc::sync_channel(0);
    let (unwound_tx, unwound_rx) = std::sync::mpsc::sync_channel(0);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(0);
    (
        ReceiveUnwindTestControl {
            received: received_rx,
            panic_now: panic_tx,
            unwound: unwound_rx,
            resume_unwind: resume_tx,
        },
        ReceiveUnwindTestInstaller(Some(ReceiveUnwindTestEndpoint {
            received: received_tx,
            panic_now: panic_rx,
            unwound: unwound_tx,
            resume_unwind: resume_rx,
        })),
    )
}

#[cfg(all(test, target_os = "macos"))]
fn pre_drain_unwind_test_hook() {
    RECEIVE_UNWIND_TEST_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook.received
                .send(())
                .expect("observe raw descriptor receipt");
            hook.panic_now.recv().expect("inject receive unwind");
            panic!("injected panic before ancillary drain");
        }
    });
}

#[cfg(all(test, target_os = "macos"))]
fn observe_receive_unwind<T>(receive: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(receive)) {
        Ok(value) => value,
        Err(payload) => {
            RECEIVE_UNWIND_TEST_HOOK.with(|hook| {
                if let Some(hook) = hook.borrow().as_ref() {
                    hook.unwound.send(()).expect("observe receive unwind");
                    hook.resume_unwind.recv().expect("resume receive unwind");
                }
            });
            std::panic::resume_unwind(payload)
        }
    }
}

#[cfg(all(not(test), target_os = "macos"))]
fn observe_receive_unwind<T>(receive: impl FnOnce() -> T) -> T {
    receive()
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[derive(Debug, PartialEq, Eq)]
struct PostRecvTestEvent {
    cloexec: Vec<bool>,
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
struct ReceiveTestEndpoint {
    events: std::sync::mpsc::SyncSender<PostRecvTestEvent>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
thread_local! {
    static RECEIVE_TEST_HOOK: std::cell::RefCell<Option<ReceiveTestEndpoint>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
struct ReceiveTestControl {
    events: std::sync::mpsc::Receiver<PostRecvTestEvent>,
    resume: std::sync::mpsc::SyncSender<()>,
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
impl ReceiveTestControl {
    fn event(&self) -> PostRecvTestEvent {
        self.events.recv().expect("post-recv hook event")
    }

    fn resume(&self) {
        self.resume.send(()).expect("resume descriptor receipt")
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
struct ReceiveTestInstaller(Option<ReceiveTestEndpoint>);

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
impl ReceiveTestInstaller {
    fn install(mut self) -> InstalledReceiveTestHook {
        let endpoint = self.0.take().expect("receive hook installed once");
        RECEIVE_TEST_HOOK.with(|hook| {
            assert!(hook.borrow_mut().replace(endpoint).is_none());
        });
        InstalledReceiveTestHook
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
struct InstalledReceiveTestHook;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
impl Drop for InstalledReceiveTestHook {
    fn drop(&mut self) {
        RECEIVE_TEST_HOOK.with(|hook| {
            hook.borrow_mut().take();
        });
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
fn receive_test_hook() -> (ReceiveTestControl, ReceiveTestInstaller) {
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(0);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(0);
    (
        ReceiveTestControl {
            events: event_rx,
            resume: resume_tx,
        },
        ReceiveTestInstaller(Some(ReceiveTestEndpoint {
            events: event_tx,
            resume: resume_rx,
        })),
    )
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
fn post_recv_test_hook(descriptors: &[OwnedFd]) {
    RECEIVE_TEST_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            let event = PostRecvTestEvent {
                cloexec: descriptors
                    .iter()
                    .map(|descriptor| {
                        fcntl_getfd(descriptor)
                            .expect("inspect received descriptor flags")
                            .contains(FdFlags::CLOEXEC)
                    })
                    .collect(),
            };
            hook.events.send(event).expect("observe descriptor receipt");
            hook.resume.recv().expect("resume descriptor receipt");
        }
    });
}

#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos")),
    allow(
        dead_code,
        reason = "unsupported targets refuse before descriptor marker receipt"
    )
)]
fn read_descriptor_message_tail<R: Read>(
    reader: &mut R,
    message: &mut [u8; DESCRIPTOR_MESSAGE.len()],
    mut filled: usize,
) -> Result<(), BootstrapError> {
    while filled < message.len() {
        let read = reader
            .read(&mut message[filled..])
            .map_err(BootstrapError::NativeTransportIo)?;
        if read == 0 {
            return Err(BootstrapError::DescriptorMessage);
        }
        filled += read;
    }
    Ok(())
}

fn bind_terminal_identity(
    connection: ConnId,
    peer: PeerIdentity,
    geometry: TerminalGeometry,
    stdin: OwnedFd,
    stdout: OwnedFd,
) -> Result<BoundTerminalDescriptors, BootstrapError> {
    let stdin_stat = fstat(&stdin).map_err(io_error)?;
    let stdout_stat = fstat(&stdout).map_err(io_error)?;
    if stdin_stat.st_dev != stdout_stat.st_dev
        || stdin_stat.st_ino != stdout_stat.st_ino
        || stdin_stat.st_rdev != stdout_stat.st_rdev
    {
        return Err(BootstrapError::DifferentTerminals);
    }

    let input_mode = fcntl_getfl(&stdin).map_err(io_error)? & OFlags::ACCMODE;
    let output_mode = fcntl_getfl(&stdout).map_err(io_error)? & OFlags::ACCMODE;
    if !descriptor_roles_allow(input_mode, output_mode) {
        return Err(BootstrapError::DescriptorRoles);
    }

    let fingerprint = TerminalFingerprint {
        device: stdin_stat.st_dev as u64,
        inode: stdin_stat.st_ino as u64,
        special_device: stdin_stat.st_rdev as u64,
    };
    Ok(BoundTerminalDescriptors {
        connection,
        peer,
        fingerprint,
        geometry,
        witness: None,
        _test_stdin: Some(stdin),
        _test_stdout: Some(stdout),
    })
}

fn bind_verified_terminal(
    connection: ConnId,
    peer: PeerIdentity,
    witness: ControllingTtyWitness,
) -> BoundTerminalDescriptors {
    let (device, inode, special_device) = witness.fingerprint().components();
    let geometry = witness.initial_geometry();
    BoundTerminalDescriptors {
        connection,
        peer,
        fingerprint: TerminalFingerprint {
            device,
            inode,
            special_device,
        },
        geometry: TerminalGeometry {
            cols: geometry.cols,
            rows: geometry.rows,
            xpixel: geometry.xpixel,
            ypixel: geometry.ypixel,
        },
        witness: Some(witness),
        _test_stdin: None,
        _test_stdout: None,
    }
}

fn descriptor_roles_allow(input_mode: OFlags, output_mode: OFlags) -> bool {
    matches!(input_mode, OFlags::RDONLY | OFlags::RDWR)
        && matches!(output_mode, OFlags::WRONLY | OFlags::RDWR)
}

/// Injectable monotonic time source used only for capability lifetime decisions.
pub(crate) trait MonotonicClock: Send + Sync + 'static {
    fn now(&self) -> Duration;
}

/// Injectable capability entropy source.
pub(crate) trait CapabilityRng: Send + Sync + 'static {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), BootstrapError>;
}

#[derive(Debug)]
struct SystemClock(Instant);

impl Default for SystemClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}

impl MonotonicClock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

struct HandshakeDeadline {
    clock: Arc<dyn MonotonicClock>,
    expires_at: Duration,
}

/// Separate budget for native preparation/publication and delivery of its launch receipt.
///
/// The bootstrap capability deadline ends when authorization is atomically consumed. A future
/// native executor receives this distinct deadline so process preparation can never silently
/// inherit Task 2's ten-second descriptor handshake budget.
pub(crate) struct NativeLaunchDeadline(HandshakeDeadline);

impl fmt::Debug for NativeLaunchDeadline {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeLaunchDeadline")
            .field("expires_at", &self.0.expires_at)
            .finish_non_exhaustive()
    }
}

impl NativeLaunchDeadline {
    fn new(clock: Arc<dyn MonotonicClock>, timeout: Duration) -> Result<Self, BootstrapError> {
        HandshakeDeadline::new(clock, timeout).map(Self)
    }

    #[allow(
        dead_code,
        reason = "the production native executor consumes this budget"
    )]
    pub(crate) fn require_remaining(&self) -> Result<Duration, BootstrapError> {
        self.0.require_remaining().map_err(|error| match error {
            BootstrapError::HandshakeExpired => BootstrapError::LaunchResultExpired,
            error => error,
        })
    }
}

fn claim_ack_deadline(
    deadline: &NativeLaunchDeadline,
) -> Result<HandshakeDeadline, BootstrapError> {
    let now = deadline.0.clock.now();
    let capped = now
        .checked_add(CLAIM_ACK_TIMEOUT)
        .ok_or(BootstrapError::ClockOverflow)?;
    let expires_at = deadline.0.expires_at.min(capped);
    if expires_at <= now {
        return Err(BootstrapError::LaunchResultExpired);
    }
    Ok(HandshakeDeadline {
        clock: Arc::clone(&deadline.0.clock),
        expires_at,
    })
}

impl HandshakeDeadline {
    fn new(clock: Arc<dyn MonotonicClock>, timeout: Duration) -> Result<Self, BootstrapError> {
        let expires_at = clock
            .now()
            .checked_add(timeout)
            .ok_or(BootstrapError::ClockOverflow)?;
        Ok(Self { clock, expires_at })
    }

    fn require_remaining(&self) -> Result<Duration, BootstrapError> {
        let remaining = self
            .expires_at
            .checked_sub(self.clock.now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(BootstrapError::HandshakeExpired)?;
        Ok(remaining)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn install_read_timeout(&self, stream: &UnixStream) -> Result<(), BootstrapError> {
        stream
            .set_read_timeout(Some(self.require_remaining()?))
            .map_err(BootstrapError::NativeTransportIo)
    }
}

trait DeadlineTransport: Read + Write {
    fn set_deadline_read_timeout(&mut self, timeout: Duration) -> std::io::Result<()>;
    fn set_deadline_write_timeout(&mut self, timeout: Duration) -> std::io::Result<()>;
}

impl DeadlineTransport for &UnixStream {
    fn set_deadline_read_timeout(&mut self, timeout: Duration) -> std::io::Result<()> {
        UnixStream::set_read_timeout(self, Some(timeout))
    }

    fn set_deadline_write_timeout(&mut self, timeout: Duration) -> std::io::Result<()> {
        UnixStream::set_write_timeout(self, Some(timeout))
    }
}

struct DeadlineIo<'deadline, T> {
    inner: T,
    deadline: &'deadline HandshakeDeadline,
}

impl<'deadline, T> DeadlineIo<'deadline, T> {
    const fn new(inner: T, deadline: &'deadline HandshakeDeadline) -> Self {
        Self { inner, deadline }
    }

    fn remaining_io(&self) -> std::io::Result<Duration> {
        self.deadline.require_remaining().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "native bootstrap handshake deadline expired",
            )
        })
    }
}

impl<T: DeadlineTransport> Read for DeadlineIo<'_, T> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.remaining_io()?;
        self.inner.set_deadline_read_timeout(remaining)?;
        self.inner.read(output)
    }
}

impl<T: DeadlineTransport> Write for DeadlineIo<'_, T> {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        let remaining = self.remaining_io()?;
        self.inner.set_deadline_write_timeout(remaining)?;
        self.inner.write(input)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let remaining = self.remaining_io()?;
        self.inner.set_deadline_write_timeout(remaining)?;
        self.inner.flush()
    }
}

#[derive(Debug)]
struct SystemRng;

impl CapabilityRng for SystemRng {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), BootstrapError> {
        getrandom::fill(bytes).map_err(|error| BootstrapError::Random(error.to_string()))
    }
}

/// Move-only random capability returned to the native client.
pub struct DirectCliCapability([u8; 32]);

impl DirectCliCapability {
    const fn wire_bytes(&self) -> [u8; 32] {
        self.0
    }
}

/// Opaque single-use authority to claim the initiating native pane's writer slot.
///
/// This value cannot authorize a launch. It is minted only after launch authorization has already
/// been consumed and is returned only with the exact [`AgentId`] whose pane reservation it names.
#[derive(PartialEq, Eq)]
pub(crate) struct NativeLaunchTicket([u8; 32]);

impl NativeLaunchTicket {
    pub(crate) fn from_reserved(bytes: [u8; 32]) -> Result<Self, BootstrapError> {
        if bytes == [0; 32] {
            return Err(BootstrapError::NativeWireProtocol);
        }
        Ok(Self(bytes))
    }

    #[cfg(test)]
    pub(crate) fn for_test(bytes: [u8; 32]) -> Self {
        assert!(bytes != [0; 32]);
        Self(bytes)
    }

    const fn wire_bytes(&self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for NativeLaunchTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeLaunchTicket([REDACTED])")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeLaunchDescriptor {
    vendor: String,
    command: String,
    adapter: String,
}

impl NativeLaunchDescriptor {
    #[cfg(test)]
    pub(crate) fn new(vendor: &str, command: &str, adapter: &str) -> Self {
        Self {
            vendor: vendor.into(),
            command: command.into(),
            adapter: adapter.into(),
        }
    }

    #[allow(
        dead_code,
        reason = "dark until an authenticated native claim transport activates"
    )]
    fn from_selected(selected: &SelectedNativeFacade<'_>) -> Self {
        let descriptor = selected.descriptor();
        let adapter = descriptor
            .native
            .expect("selected native launch contains a native lane")
            .adapter()
            .as_str();
        Self {
            vendor: descriptor.identity.as_str().into(),
            command: descriptor.command.into(),
            adapter: adapter.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeLaunchBinding {
    agent_id: AgentId,
    project: PathBuf,
    principal: PeerIdentity,
    bootstrap_conn: ConnId,
    terminal: TerminalFingerprint,
    initial_geometry: TerminalGeometry,
    descriptor: NativeLaunchDescriptor,
    context_hash: ContextHash,
}

#[allow(
    dead_code,
    reason = "dark until an authenticated native claim transport activates"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeLaunchBindingSeed {
    project: PathBuf,
    principal: PeerIdentity,
    bootstrap_conn: ConnId,
    terminal: TerminalFingerprint,
    initial_geometry: TerminalGeometry,
    context_hash: ContextHash,
}

#[allow(
    dead_code,
    reason = "dark until an authenticated native claim transport activates"
)]
pub(crate) struct SelectedNativeLaunch<'registry> {
    selected: SelectedNativeFacade<'registry>,
    terminal: ControllingTtyWitness,
    binding: NativeLaunchBindingSeed,
}

#[allow(
    dead_code,
    reason = "dark until an authenticated native claim transport activates"
)]
impl<'registry> SelectedNativeLaunch<'registry> {
    pub(crate) fn into_parts(
        self,
        agent_id: AgentId,
    ) -> (
        SelectedNativeFacade<'registry>,
        ControllingTtyWitness,
        NativeLaunchBinding,
    ) {
        let descriptor = NativeLaunchDescriptor::from_selected(&self.selected);
        let binding = NativeLaunchBinding {
            agent_id,
            project: self.binding.project,
            principal: self.binding.principal,
            bootstrap_conn: self.binding.bootstrap_conn,
            terminal: self.binding.terminal,
            initial_geometry: self.binding.initial_geometry,
            descriptor,
            context_hash: self.binding.context_hash,
        };
        (self.selected, self.terminal, binding)
    }
}

#[allow(
    dead_code,
    reason = "dark until an authenticated native claim transport activates"
)]
pub(crate) fn select_authenticated_native_launch<'registry>(
    registry: &'registry marion_core::NativeFacadeRegistry<'_>,
    consumed: ConsumedNativeRequest<'_>,
) -> Option<SelectedNativeLaunch<'registry>> {
    let (context, hash, terminal, authorization) = consumed.into_parts().into_components();
    let binding = NativeLaunchBindingSeed {
        project: context.canonical_project.clone(),
        principal: terminal.peer,
        bootstrap_conn: terminal.connection,
        terminal: terminal.fingerprint,
        initial_geometry: terminal.geometry,
        context_hash: hash,
    };
    let selected = select_consumed_native(registry, authorization)?;
    let terminal = terminal.into_controlling_tty_witness()?;
    #[cfg(test)]
    observe_native_route_test_stage(NativeRouteTestStage::SelectorSelected);
    Some(SelectedNativeLaunch {
        selected,
        terminal,
        binding,
    })
}

impl NativeLaunchBinding {
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        agent_id: AgentId,
        project: PathBuf,
        principal: PeerIdentity,
        bootstrap_conn: ConnId,
        terminal: TerminalFingerprint,
        initial_geometry: TerminalGeometry,
        descriptor: NativeLaunchDescriptor,
        context_hash: ContextHash,
    ) -> Self {
        Self {
            agent_id,
            project,
            principal,
            bootstrap_conn,
            terminal,
            initial_geometry,
            descriptor,
            context_hash,
        }
    }

    #[allow(
        dead_code,
        reason = "dark until an authenticated native claim transport activates"
    )]
    pub(crate) const fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }
}

#[allow(
    dead_code,
    reason = "dark until an authenticated native claim transport activates"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeLaunchClaim {
    agent_id: AgentId,
    host_generation: u64,
    conn: ConnId,
}

/// Sealed identity of a future ticket-presenting attach connection.
///
/// There is intentionally no production constructor yet. Activation requires a claim transport
/// that derives both uid and pid from kernel credentials and calls `gone(conn)` on departure; the
/// ordinary JSON-RPC socket currently authenticates uid only and cannot mint this value.
#[allow(
    dead_code,
    reason = "dark until an authenticated native claim transport activates"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NativeClaimant {
    conn: ConnId,
    principal: PeerIdentity,
}

#[allow(
    dead_code,
    reason = "dark until an authenticated native claim transport activates"
)]
impl NativeClaimant {
    #[cfg(test)]
    pub(crate) const fn new(conn: ConnId, principal: PeerIdentity) -> Self {
        Self { conn, principal }
    }

    pub(crate) const fn conn(self) -> ConnId {
        self.conn
    }

    const fn from_authenticated_connection(conn: ConnId, principal: PeerIdentity) -> Self {
        Self { conn, principal }
    }
}

#[allow(
    dead_code,
    reason = "dark until an authenticated native claim transport activates"
)]
impl NativeLaunchClaim {
    pub(crate) const fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub(crate) const fn conn(&self) -> ConnId {
        self.conn
    }

    pub(crate) const fn host_generation(&self) -> u64 {
        self.host_generation
    }
}

#[allow(
    dead_code,
    reason = "dark until an authenticated native claim transport activates"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum NativeLaunchReservationError {
    #[error("native launch ticket entropy was exhausted")]
    TicketGenerationExhausted,
    #[error("native launch clock overflowed")]
    ClockOverflow,
    #[error("native launch receipt was not reserved here")]
    UnknownTicket,
    #[error("native launch reservation was already published")]
    AlreadyPublished,
    #[error("native launch pane is not live")]
    HostUnavailable,
    #[error("native launch pane is already visible")]
    HostAlreadyVisible,
}

#[allow(
    dead_code,
    reason = "dark until an authenticated native claim transport activates"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum NativeLaunchClaimError {
    #[error("native launch ticket is unknown or already consumed")]
    UnknownTicket,
    #[error("native launch reservation has not been published")]
    Unpublished,
    #[error("native launch ticket binding does not match")]
    WrongBinding,
    #[error("native launch ticket expired")]
    Expired,
    #[error("native pane writer is already held")]
    WriterBusy,
}

#[allow(
    dead_code,
    reason = "dark until an authenticated native claim transport activates"
)]
struct PendingNativeLaunch {
    binding: NativeLaunchBinding,
    host_generation: Option<u64>,
    expires_at: Option<Duration>,
}

struct PendingNativeLaunchState {
    tickets: HashMap<[u8; 32], PendingNativeLaunch>,
    claiming: HashMap<[u8; 32], PendingNativeLaunch>,
    agents: HashMap<AgentId, [u8; 32]>,
}

#[allow(
    dead_code,
    reason = "dark until an authenticated native claim transport activates"
)]
pub(crate) struct PendingNativeLaunches {
    state: Mutex<PendingNativeLaunchState>,
    rng: Arc<dyn CapabilityRng>,
    clock: Arc<dyn MonotonicClock>,
    ttl: Duration,
}

#[allow(
    dead_code,
    reason = "dark until an authenticated native claim transport activates"
)]
impl PendingNativeLaunches {
    const TICKET_ATTEMPTS: usize = 8;

    #[cfg(test)]
    pub(crate) fn with_sources(
        rng: Arc<dyn CapabilityRng>,
        clock: Arc<dyn MonotonicClock>,
        ttl: Duration,
    ) -> Self {
        Self {
            state: Mutex::new(PendingNativeLaunchState {
                tickets: HashMap::new(),
                claiming: HashMap::new(),
                agents: HashMap::new(),
            }),
            rng,
            clock,
            ttl,
        }
    }

    pub(crate) fn reserve(
        self: &Arc<Self>,
        binding: NativeLaunchBinding,
    ) -> Result<PendingNativeLaunchReceipt, NativeLaunchReservationError> {
        let mut state = lock(&self.state);
        let mut ticket = None;
        for _ in 0..Self::TICKET_ATTEMPTS {
            let mut candidate = [0; 32];
            if self.rng.fill(&mut candidate).is_err() {
                continue;
            }
            if candidate != [0; 32]
                && !state.tickets.contains_key(&candidate)
                && !state.claiming.contains_key(&candidate)
            {
                ticket = Some(candidate);
                break;
            }
        }
        let ticket = ticket.ok_or(NativeLaunchReservationError::TicketGenerationExhausted)?;
        if let Some(old) = state.agents.insert(binding.agent_id.clone(), ticket) {
            state.tickets.remove(&old);
            state.claiming.remove(&old);
        }
        state.tickets.insert(
            ticket,
            PendingNativeLaunch {
                binding: binding.clone(),
                host_generation: None,
                expires_at: None,
            },
        );
        let receipt = NativeLaunchReceipt::new(
            binding.agent_id,
            NativeLaunchTicket::from_reserved(ticket)
                .expect("ticket generation rejects the reserved all-zero value"),
        );
        let authority = Arc::clone(self);
        Ok(PendingNativeLaunchReceipt::new(receipt, move || {
            authority.revoke_ticket(ticket);
        }))
    }

    pub(crate) fn publish(
        &self,
        receipt: &NativeLaunchReceipt,
        host_generation: u64,
    ) -> Result<(), NativeLaunchReservationError> {
        let expires_at = self
            .clock
            .now()
            .checked_add(self.ttl)
            .ok_or(NativeLaunchReservationError::ClockOverflow)?;
        let mut state = lock(&self.state);
        let pending = state
            .tickets
            .get_mut(&receipt.ticket().wire_bytes())
            .filter(|pending| pending.binding.agent_id() == receipt.agent_id())
            .ok_or(NativeLaunchReservationError::UnknownTicket)?;
        if pending.host_generation.is_some() || pending.expires_at.is_some() {
            return Err(NativeLaunchReservationError::AlreadyPublished);
        }
        pending.host_generation = Some(host_generation);
        pending.expires_at = Some(expires_at);
        Ok(())
    }

    pub(crate) fn has_pending(&self, agent: &AgentId) -> bool {
        let now = self.clock.now();
        let mut state = lock(&self.state);
        let Some(ticket) = state.agents.get(agent).copied() else {
            return false;
        };
        let expired = state
            .tickets
            .get(&ticket)
            .and_then(|pending| pending.expires_at)
            .is_some_and(|expires_at| now >= expires_at);
        if expired {
            state.agents.remove(agent);
            state.tickets.remove(&ticket);
            return false;
        }
        true
    }

    pub(crate) fn consume(
        self: &Arc<Self>,
        ticket: &NativeLaunchTicket,
        agent_id: &AgentId,
        host_generation: u64,
        claimant: NativeClaimant,
    ) -> Result<NativeLaunchClaim, NativeLaunchClaimError> {
        self.claim_with(ticket, agent_id, host_generation, claimant, || Ok(()))
            .map(|(claim, ())| claim)
    }

    pub(crate) fn claim_with<T>(
        self: &Arc<Self>,
        ticket: &NativeLaunchTicket,
        agent_id: &AgentId,
        host_generation: u64,
        claimant: NativeClaimant,
        claim: impl FnOnce() -> Result<T, NativeLaunchClaimError>,
    ) -> Result<(NativeLaunchClaim, T), NativeLaunchClaimError> {
        let pending = self.prepare_claim(ticket, agent_id, host_generation, claimant)?;
        let value = claim()?;
        let claim = pending.commit()?;
        Ok((claim, value))
    }

    pub(crate) fn prepare_claim(
        self: &Arc<Self>,
        ticket: &NativeLaunchTicket,
        agent_id: &AgentId,
        host_generation: u64,
        claimant: NativeClaimant,
    ) -> Result<PreparedNativeLaunchClaim, NativeLaunchClaimError> {
        let key = ticket.wire_bytes();
        let now = self.clock.now();
        let mut state = lock(&self.state);
        let pending = state
            .tickets
            .get(&key)
            .ok_or(NativeLaunchClaimError::UnknownTicket)?;
        let Some(expires_at) = pending.expires_at else {
            return Err(NativeLaunchClaimError::Unpublished);
        };
        if now >= expires_at {
            let expired = state.tickets.remove(&key).expect("looked up above");
            state.agents.remove(expired.binding.agent_id());
            return Err(NativeLaunchClaimError::Expired);
        }
        if pending.binding.agent_id() != agent_id
            || pending.host_generation != Some(host_generation)
            || pending.binding.principal != claimant.principal
        {
            return Err(NativeLaunchClaimError::WrongBinding);
        }
        let claiming = state.tickets.remove(&key).expect("looked up above");
        assert!(state.claiming.insert(key, claiming).is_none());
        Ok(PreparedNativeLaunchClaim {
            authority: Arc::clone(self),
            key,
            agent_id: agent_id.clone(),
            host_generation,
            conn: claimant.conn,
            finished: false,
        })
    }

    pub(crate) fn revoke_agent(&self, agent: &AgentId) {
        let mut state = lock(&self.state);
        if let Some(ticket) = state.agents.remove(agent) {
            state.tickets.remove(&ticket);
            state.claiming.remove(&ticket);
        }
    }

    fn revoke_ticket(&self, ticket: [u8; 32]) {
        let mut state = lock(&self.state);
        if let Some(pending) = state.tickets.remove(&ticket)
            && state.agents.get(pending.binding.agent_id()) == Some(&ticket)
        {
            state.agents.remove(pending.binding.agent_id());
        }
        if let Some(pending) = state.claiming.remove(&ticket)
            && state.agents.get(pending.binding.agent_id()) == Some(&ticket)
        {
            state.agents.remove(pending.binding.agent_id());
        }
    }
}

pub(crate) struct PreparedNativeLaunchClaim {
    authority: Arc<PendingNativeLaunches>,
    key: [u8; 32],
    agent_id: AgentId,
    host_generation: u64,
    conn: ConnId,
    finished: bool,
}

impl PreparedNativeLaunchClaim {
    pub(crate) fn commit(mut self) -> Result<NativeLaunchClaim, NativeLaunchClaimError> {
        let mut state = lock(&self.authority.state);
        let pending = state
            .claiming
            .get(&self.key)
            .ok_or(NativeLaunchClaimError::WrongBinding)?;
        if pending.binding.agent_id() != &self.agent_id
            || pending.host_generation != Some(self.host_generation)
            || state.agents.get(&self.agent_id) != Some(&self.key)
        {
            return Err(NativeLaunchClaimError::WrongBinding);
        }
        let pending = state
            .claiming
            .remove(&self.key)
            .expect("validated claiming reservation remains under the same lock");
        state.agents.remove(&self.agent_id);
        self.finished = true;
        Ok(NativeLaunchClaim {
            agent_id: pending.binding.agent_id,
            host_generation: self.host_generation,
            conn: self.conn,
        })
    }
}

impl Drop for PreparedNativeLaunchClaim {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut state = lock(&self.authority.state);
        let Some(pending) = state.claiming.remove(&self.key) else {
            return;
        };
        if state.agents.get(&self.agent_id) == Some(&self.key) {
            state.tickets.insert(self.key, pending);
        }
    }
}

/// Successful native bootstrap output. Identity and writer priority always travel together.
#[derive(Debug)]
pub struct NativeLaunchReceipt {
    agent_id: AgentId,
    ticket: NativeLaunchTicket,
}

impl NativeLaunchReceipt {
    pub(crate) const fn new(agent_id: AgentId, ticket: NativeLaunchTicket) -> Self {
        Self { agent_id, ticket }
    }

    pub const fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub(crate) const fn ticket(&self) -> &NativeLaunchTicket {
        &self.ticket
    }
}

/// Receipt whose pane reservation is revoked unless the complete response reaches the client.
pub(crate) struct PendingNativeLaunchReceipt {
    receipt: NativeLaunchReceipt,
    cancel: Option<Box<dyn FnOnce() + Send>>,
}

impl PendingNativeLaunchReceipt {
    #[allow(
        dead_code,
        reason = "dark until an authenticated native claim transport activates"
    )]
    pub(crate) fn new(
        receipt: NativeLaunchReceipt,
        cancel: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            receipt,
            cancel: Some(Box::new(cancel)),
        }
    }

    pub(crate) const fn receipt(&self) -> &NativeLaunchReceipt {
        &self.receipt
    }

    /// Add request-local rollback to the ticket's existing authority revocation. The service
    /// disarms both only after claim acknowledgement and relay commit.
    pub(crate) fn on_cancel(&mut self, cancel: impl FnOnce() + Send + 'static) {
        let authority = self.cancel.take();
        self.cancel = Some(Box::new(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(cancel));
            if let Some(authority) = authority {
                authority();
            }
        }));
    }

    fn commit(mut self) {
        self.cancel.take();
    }
}

impl fmt::Debug for PendingNativeLaunchReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingNativeLaunchReceipt")
            .field("receipt", &self.receipt)
            .field("cancel", &self.cancel.as_ref().map(|_| "armed"))
            .finish()
    }
}

impl Drop for PendingNativeLaunchReceipt {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(cancel));
        }
    }
}

/// Move-only proof that the native authority completed an atomic capability consume.
///
/// The field and constructor are private to this module. Other supervisor modules may accept the
/// proof, but cannot manufacture one.
pub(crate) struct ConsumedNativeCapability<'selector> {
    selector: &'selector str,
}

impl<'selector> ConsumedNativeCapability<'selector> {
    fn new(selector: &'selector str) -> Self {
        Self { selector }
    }

    pub(crate) const fn selector(&self) -> &'selector str {
        self.selector
    }
}

const WIRE_ISSUE: u8 = 1;
const WIRE_CONSUME: u8 = 2;
const WIRE_OK: u8 = 0;
const WIRE_REFUSED: u8 = 1;

enum WireRequest {
    Issue {
        context: DirectNativeRequestContext,
        claimed_hash: ContextHash,
    },
    Consume {
        context: DirectNativeRequestContext,
        claimed_hash: ContextHash,
        token: [u8; 32],
    },
}

struct AuthenticatedIssue {
    context: DirectNativeRequestContext,
    hash: ContextHash,
}

struct AuthenticatedConsume {
    context: DirectNativeRequestContext,
    hash: ContextHash,
    token: [u8; 32],
}

impl WireRequest {
    fn authenticate_issue(self) -> Result<AuthenticatedIssue, BootstrapError> {
        let Self::Issue {
            context,
            claimed_hash,
        } = self
        else {
            return Err(BootstrapError::NativeWireProtocol);
        };
        let hash = authenticate_context(&context, claimed_hash)?;
        Ok(AuthenticatedIssue { context, hash })
    }

    fn authenticate_consume(self) -> Result<AuthenticatedConsume, BootstrapError> {
        let Self::Consume {
            context,
            claimed_hash,
            token,
        } = self
        else {
            return Err(BootstrapError::NativeWireProtocol);
        };
        let hash = authenticate_context(&context, claimed_hash)?;
        Ok(AuthenticatedConsume {
            context,
            hash,
            token,
        })
    }

    #[cfg(test)]
    fn authenticated_context(
        self,
    ) -> Result<(DirectNativeRequestContext, ContextHash, Option<[u8; 32]>), BootstrapError> {
        let (context, claimed_hash, token) = match self {
            WireRequest::Issue {
                context,
                claimed_hash,
            } => (context, claimed_hash, None),
            WireRequest::Consume {
                context,
                claimed_hash,
                token,
            } => (context, claimed_hash, Some(token)),
        };
        let computed = authenticate_context(&context, claimed_hash)?;
        Ok((context, computed, token))
    }
}

fn authenticate_context(
    context: &DirectNativeRequestContext,
    claimed_hash: ContextHash,
) -> Result<ContextHash, BootstrapError> {
    let computed = context_hash(context);
    if computed != claimed_hash {
        return Err(BootstrapError::NativeWireContextHashMismatch);
    }
    Ok(computed)
}

fn context_selector(context: &DirectNativeRequestContext) -> Result<&str, BootstrapError> {
    let selector = context
        .selector
        .to_str()
        .ok_or(BootstrapError::NativeWireProtocol)?;
    if selector.is_empty() || !selector.is_ascii() || selector.len() > MAX_SELECTOR_BYTES {
        return Err(BootstrapError::NativeWireProtocol);
    }
    Ok(selector)
}

/// Native-client half of the descriptor-bearing capability request.
#[allow(
    dead_code,
    reason = "Task 3 calls this only after constructing its crate-private ClientTtyWitness"
)]
pub(crate) fn request_direct_cli_capability(
    stream: &mut UnixStream,
    stdin: BorrowedFd<'_>,
    stdout: BorrowedFd<'_>,
    context: &DirectNativeRequestContext,
) -> Result<DirectCliCapability, BootstrapError> {
    send_terminal_descriptors(stream, stdin, stdout)?;
    write_wire_request(
        stream,
        &WireRequest::Issue {
            context: context.clone(),
            claimed_hash: context_hash(context),
        },
    )?;
    let mut status = [0u8; 1];
    stream
        .read_exact(&mut status)
        .map_err(BootstrapError::NativeTransportIo)?;
    if status[0] != WIRE_OK {
        return Err(BootstrapError::AuthorizationRefused);
    }
    let mut token = [0u8; 32];
    stream
        .read_exact(&mut token)
        .map_err(BootstrapError::NativeTransportIo)?;
    Ok(DirectCliCapability(token))
}

/// Present a capability on the same native connection for atomic consumption.
pub fn present_direct_cli_capability(
    stream: &mut UnixStream,
    capability: DirectCliCapability,
    context: &DirectNativeRequestContext,
) -> Result<NativeLaunchReceipt, BootstrapError> {
    write_wire_request(
        stream,
        &WireRequest::Consume {
            context: context.clone(),
            claimed_hash: context_hash(context),
            token: capability.0,
        },
    )?;
    let mut status = [0u8; 1];
    stream
        .read_exact(&mut status)
        .map_err(BootstrapError::NativeTransportIo)?;
    if status[0] != WIRE_OK {
        return Err(BootstrapError::AuthorizationRefused);
    }
    let mut length = [0u8; 2];
    stream
        .read_exact(&mut length)
        .map_err(BootstrapError::NativeTransportIo)?;
    let length = usize::from(u16::from_be_bytes(length));
    if length != UUID_LEN {
        return Err(BootstrapError::NativeWireProtocol);
    }
    let mut agent_id = vec![0u8; length];
    stream
        .read_exact(&mut agent_id)
        .map_err(BootstrapError::NativeTransportIo)?;
    let agent_id = String::from_utf8(agent_id).map_err(|_| BootstrapError::NativeWireProtocol)?;
    if !is_uuid_v7(&agent_id) {
        return Err(BootstrapError::NativeWireProtocol);
    }
    let mut ticket = [0u8; 32];
    stream
        .read_exact(&mut ticket)
        .map_err(BootstrapError::NativeTransportIo)?;
    Ok(NativeLaunchReceipt::new(
        AgentId(agent_id),
        NativeLaunchTicket::from_reserved(ticket)?,
    ))
}

fn write_native_launch_receipt(
    stream: &mut impl Write,
    receipt: &NativeLaunchReceipt,
) -> Result<(), BootstrapError> {
    let agent_id = receipt.agent_id().0.as_bytes();
    if !is_uuid_v7(&receipt.agent_id().0) {
        return Err(BootstrapError::NativeWireProtocol);
    }
    let length = u16::try_from(UUID_LEN).expect("UUID length fits the wire field");
    stream
        .write_all(&[WIRE_OK])
        .map_err(BootstrapError::NativeTransportIo)?;
    stream
        .write_all(&length.to_be_bytes())
        .map_err(BootstrapError::NativeTransportIo)?;
    stream
        .write_all(agent_id)
        .map_err(BootstrapError::NativeTransportIo)?;
    stream
        .write_all(&receipt.ticket().wire_bytes())
        .map_err(BootstrapError::NativeTransportIo)?;
    stream.flush().map_err(BootstrapError::NativeTransportIo)
}

#[derive(Debug, PartialEq, Eq)]
struct NativeClaimRequest {
    agent_id: AgentId,
    ticket: NativeLaunchTicket,
}

fn write_native_claim_request(
    stream: &mut impl Write,
    receipt: &NativeLaunchReceipt,
) -> Result<(), BootstrapError> {
    if !is_uuid_v7(&receipt.agent_id().0) {
        return Err(BootstrapError::NativeWireProtocol);
    }
    stream
        .write_all(CLAIM_MESSAGE)
        .and_then(|()| stream.write_all(receipt.agent_id().0.as_bytes()))
        .and_then(|()| stream.write_all(&receipt.ticket().wire_bytes()))
        .and_then(|()| stream.flush())
        .map_err(BootstrapError::NativeTransportIo)
}

fn read_native_claim_request(stream: &mut impl Read) -> Result<NativeClaimRequest, BootstrapError> {
    let mut magic = [0; CLAIM_MESSAGE.len()];
    stream
        .read_exact(&mut magic)
        .map_err(BootstrapError::NativeTransportIo)?;
    if magic != CLAIM_MESSAGE {
        return Err(BootstrapError::NativeWireProtocol);
    }
    let mut agent_id = [0; UUID_LEN];
    stream
        .read_exact(&mut agent_id)
        .map_err(BootstrapError::NativeTransportIo)?;
    let agent_id =
        String::from_utf8(agent_id.to_vec()).map_err(|_| BootstrapError::NativeWireProtocol)?;
    if !is_uuid_v7(&agent_id) {
        return Err(BootstrapError::NativeWireProtocol);
    }
    let mut ticket = [0; 32];
    stream
        .read_exact(&mut ticket)
        .map_err(BootstrapError::NativeTransportIo)?;
    Ok(NativeClaimRequest {
        agent_id: AgentId(agent_id),
        ticket: NativeLaunchTicket::from_reserved(ticket)?,
    })
}

/// The exact bytes a native client puts on the wire for an `Issue`, for tests that must prove an
/// ordinary transport refuses this frame before any capability lookup.
#[cfg(test)]
pub(crate) fn issue_request_bytes_for_tests(context: &DirectNativeRequestContext) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_wire_request(
        &mut bytes,
        &WireRequest::Issue {
            context: context.clone(),
            claimed_hash: context_hash(context),
        },
    )
    .expect("an in-memory wire frame encodes");
    bytes
}

fn write_wire_request(
    stream: &mut impl Write,
    request: &WireRequest,
) -> Result<(), BootstrapError> {
    let (kind, context, claimed_hash, token) = match request {
        WireRequest::Issue {
            context,
            claimed_hash,
        } => (WIRE_ISSUE, context, claimed_hash, None),
        WireRequest::Consume {
            context,
            claimed_hash,
            token,
        } => (WIRE_CONSUME, context, claimed_hash, Some(token)),
    };
    checked_context_wire_size(context)?;
    stream
        .write_all(&[kind])
        .map_err(BootstrapError::NativeTransportIo)?;
    write_context(stream, context)?;
    stream
        .write_all(&claimed_hash.0)
        .map_err(BootstrapError::NativeTransportIo)?;
    if let Some(token) = token {
        stream
            .write_all(token)
            .map_err(BootstrapError::NativeTransportIo)?;
    }
    stream.flush().map_err(BootstrapError::NativeTransportIo)
}

fn write_context<W: Write>(
    stream: &mut W,
    context: &DirectNativeRequestContext,
) -> Result<(), BootstrapError> {
    fn field<W: Write>(stream: &mut W, value: &OsStr) -> Result<(), BootstrapError> {
        let value = value.as_bytes();
        let length = u32::try_from(value.len()).map_err(|_| BootstrapError::NativeWireProtocol)?;
        if value.len() > MAX_CONTEXT_FIELD_BYTES {
            return Err(BootstrapError::NativeWireProtocol);
        }
        stream
            .write_all(&length.to_be_bytes())
            .map_err(BootstrapError::NativeTransportIo)?;
        stream
            .write_all(value)
            .map_err(BootstrapError::NativeTransportIo)
    }

    checked_context_wire_size(context)?;
    field(stream, context.canonical_project.as_os_str())?;
    field(stream, &context.selector)?;
    stream
        .write_all(&(context.opaque_tail.len() as u32).to_be_bytes())
        .map_err(BootstrapError::NativeTransportIo)?;
    for argument in &context.opaque_tail {
        field(stream, argument)?;
    }
    field(stream, &context.terminal_profile)?;
    stream
        .write_all(&context.native_wire_version.to_be_bytes())
        .map_err(BootstrapError::NativeTransportIo)?;
    // The environment trails the hashed fields so the authenticated prefix keeps its layout; it
    // is bounded like the tail and never carries reserved identity onto the wire.
    stream
        .write_all(&(context.environment.len() as u32).to_be_bytes())
        .map_err(BootstrapError::NativeTransportIo)?;
    for (name, value) in &context.environment {
        if is_reserved_marion_name(name) {
            return Err(BootstrapError::NativeWireProtocol);
        }
        field(stream, name)?;
        field(stream, value)?;
    }
    Ok(())
}

fn read_wire_request<R: Read>(mut stream: R) -> Result<WireRequest, BootstrapError> {
    let mut kind = [0u8; 1];
    stream
        .read_exact(&mut kind)
        .map_err(BootstrapError::NativeTransportIo)?;
    let context = read_context(&mut stream)?;
    let mut hash = [0u8; 32];
    stream
        .read_exact(&mut hash)
        .map_err(BootstrapError::NativeTransportIo)?;
    match kind[0] {
        WIRE_ISSUE => Ok(WireRequest::Issue {
            context,
            claimed_hash: ContextHash(hash),
        }),
        WIRE_CONSUME => {
            let mut token = [0u8; 32];
            stream
                .read_exact(&mut token)
                .map_err(BootstrapError::NativeTransportIo)?;
            Ok(WireRequest::Consume {
                context,
                claimed_hash: ContextHash(hash),
                token,
            })
        }
        _ => Err(BootstrapError::NativeWireProtocol),
    }
}

fn read_context<R: Read>(stream: &mut R) -> Result<DirectNativeRequestContext, BootstrapError> {
    fn field<R: Read>(
        stream: &mut R,
        budget: &mut ContextBudget,
    ) -> Result<OsString, BootstrapError> {
        budget.consume(4)?;
        let mut length = [0u8; 4];
        stream
            .read_exact(&mut length)
            .map_err(BootstrapError::NativeTransportIo)?;
        let length = u32::from_be_bytes(length) as usize;
        if length > MAX_CONTEXT_FIELD_BYTES {
            return Err(BootstrapError::NativeWireProtocol);
        }
        budget.consume(length)?;
        let mut value = vec![0; length];
        stream
            .read_exact(&mut value)
            .map_err(BootstrapError::NativeTransportIo)?;
        Ok(OsString::from_vec(value))
    }

    let mut budget = ContextBudget::new();
    let canonical_project = PathBuf::from(field(stream, &mut budget)?);
    let selector = field(stream, &mut budget)?;
    budget.consume(4)?;
    let mut argument_count = [0u8; 4];
    stream
        .read_exact(&mut argument_count)
        .map_err(BootstrapError::NativeTransportIo)?;
    let argument_count = u32::from_be_bytes(argument_count) as usize;
    if argument_count > MAX_CONTEXT_ARGUMENTS {
        return Err(BootstrapError::NativeWireProtocol);
    }
    let mut opaque_tail = Vec::new();
    for _ in 0..argument_count {
        opaque_tail.push(field(stream, &mut budget)?);
    }
    let terminal_profile = field(stream, &mut budget)?;
    budget.consume(4)?;
    let mut native_wire_version = [0u8; 4];
    stream
        .read_exact(&mut native_wire_version)
        .map_err(BootstrapError::NativeTransportIo)?;
    budget.consume(4)?;
    let mut environment_count = [0u8; 4];
    stream
        .read_exact(&mut environment_count)
        .map_err(BootstrapError::NativeTransportIo)?;
    let environment_count = u32::from_be_bytes(environment_count) as usize;
    if environment_count > MAX_CONTEXT_ENVIRONMENT_ENTRIES {
        return Err(BootstrapError::NativeWireProtocol);
    }
    let mut environment = Vec::new();
    for _ in 0..environment_count {
        let name = field(stream, &mut budget)?;
        // A conforming client strips reserved identity before sending, so its presence is a
        // protocol violation rather than something to silently drop on the server's behalf.
        if is_reserved_marion_name(&name) {
            return Err(BootstrapError::NativeWireProtocol);
        }
        let value = field(stream, &mut budget)?;
        environment.push((name, value));
    }
    Ok(DirectNativeRequestContext {
        canonical_project,
        selector,
        opaque_tail,
        terminal_profile,
        native_wire_version: u32::from_be_bytes(native_wire_version),
        environment,
    })
}

struct ContextBudget {
    remaining: usize,
}

impl ContextBudget {
    const fn new() -> Self {
        Self {
            remaining: MAX_CONTEXT_BYTES,
        }
    }

    fn consume(&mut self, bytes: usize) -> Result<(), BootstrapError> {
        self.remaining = self
            .remaining
            .checked_sub(bytes)
            .ok_or(BootstrapError::NativeWireContextTooLarge)?;
        Ok(())
    }
}

fn checked_framed_context_size(
    field_lengths: impl IntoIterator<Item = usize>,
    argument_count: usize,
    environment_count: usize,
) -> Result<usize, BootstrapError> {
    if argument_count > MAX_CONTEXT_ARGUMENTS || environment_count > MAX_CONTEXT_ENVIRONMENT_ENTRIES
    {
        return Err(BootstrapError::NativeWireProtocol);
    }
    // Fixed-width framing: argument count, wire version, environment entry count.
    let mut size = 12usize;
    for length in field_lengths {
        if length > MAX_CONTEXT_FIELD_BYTES {
            return Err(BootstrapError::NativeWireProtocol);
        }
        size = size
            .checked_add(4)
            .and_then(|size| size.checked_add(length))
            .ok_or(BootstrapError::NativeWireContextTooLarge)?;
    }
    if size > MAX_CONTEXT_BYTES {
        return Err(BootstrapError::NativeWireContextTooLarge);
    }
    Ok(size)
}

fn checked_context_wire_size(
    context: &DirectNativeRequestContext,
) -> Result<usize, BootstrapError> {
    checked_framed_context_size(
        std::iter::once(context.canonical_project.as_os_str().as_bytes().len())
            .chain(std::iter::once(context.selector.as_bytes().len()))
            .chain(
                context
                    .opaque_tail
                    .iter()
                    .map(|value| value.as_bytes().len()),
            )
            .chain(std::iter::once(context.terminal_profile.as_bytes().len()))
            .chain(
                context
                    .environment
                    .iter()
                    .flat_map(|(name, value)| [name.as_bytes().len(), value.as_bytes().len()]),
            ),
        context.opaque_tail.len(),
        context.environment.len(),
    )
}

/// Opaque handoff returned only by Task 3's crate-internal terminal verifier.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TerminalGeometryObservation {
    geometry: TerminalGeometry,
}

/// Move-only downstream handoff proving the request context was authenticated by this service,
/// matched its project/version policy, and consumed a capability bound to the same connection.
#[allow(
    dead_code,
    reason = "Task 3 consumes this sealed handoff; Task 2 tests its construction and contents"
)]
pub(crate) struct ConsumedNativeRequest<'request> {
    context: &'request DirectNativeRequestContext,
    hash: ContextHash,
    terminal: BoundTerminalDescriptors,
    authorization: AuthorizedNativeFacade<'request>,
}

/// Crate-only ownership bundle used by Task 3 after the handler consumes the sealed request.
pub(crate) struct ConsumedNativeRequestParts<'request> {
    context: &'request DirectNativeRequestContext,
    hash: ContextHash,
    terminal: BoundTerminalDescriptors,
    authorization: AuthorizedNativeFacade<'request>,
}

#[allow(
    dead_code,
    reason = "Task 3 consumes these authenticated fields through this sealed read-only surface"
)]
impl<'request> ConsumedNativeRequest<'request> {
    pub(crate) const fn context(&self) -> &'request DirectNativeRequestContext {
        self.context
    }

    pub(crate) const fn hash(&self) -> ContextHash {
        self.hash
    }

    pub(crate) const fn authorization(&self) -> &AuthorizedNativeFacade<'request> {
        &self.authorization
    }

    /// The client's stripped environment, authenticated as part of this request's frame but never
    /// as a hash input.
    pub(crate) fn environment(&self) -> &'request [(OsString, OsString)] {
        self.context.environment()
    }

    pub(crate) fn into_parts(self) -> ConsumedNativeRequestParts<'request> {
        ConsumedNativeRequestParts {
            context: self.context,
            hash: self.hash,
            terminal: self.terminal,
            authorization: self.authorization,
        }
    }
}

#[allow(
    dead_code,
    reason = "Task 3 consumes this ownership bundle to bind native selection and execution"
)]
impl<'request> ConsumedNativeRequestParts<'request> {
    pub(crate) const fn context(&self) -> &'request DirectNativeRequestContext {
        self.context
    }

    pub(crate) const fn hash(&self) -> ContextHash {
        self.hash
    }

    pub(crate) const fn terminal(&self) -> &BoundTerminalDescriptors {
        &self.terminal
    }

    pub(crate) fn into_components(
        self,
    ) -> (
        &'request DirectNativeRequestContext,
        ContextHash,
        BoundTerminalDescriptors,
        AuthorizedNativeFacade<'request>,
    ) {
        (self.context, self.hash, self.terminal, self.authorization)
    }
}

impl TerminalGeometryObservation {
    #[allow(dead_code, reason = "Task 3 supplies the terminal verifier handoff")]
    pub(crate) const fn new(geometry: TerminalGeometry) -> Self {
        Self { geometry }
    }
}

pub(crate) trait NativeBootstrapHandler: Send + Sync + 'static {
    fn verify_terminal(
        &self,
        peer: PeerIdentity,
        stdin: BorrowedFd<'_>,
        stdout: BorrowedFd<'_>,
    ) -> Result<TerminalGeometryObservation, BootstrapError>;

    fn authorized(
        &self,
        request: ConsumedNativeRequest<'_>,
        deadline: &NativeLaunchDeadline,
    ) -> Result<PendingNativeLaunchReceipt, BootstrapError>;

    fn requires_claim_transport(&self) -> bool {
        false
    }

    fn prepare_native_claim(
        &self,
        _ticket: &NativeLaunchTicket,
        _agent_id: &AgentId,
        _claimant: NativeClaimant,
    ) -> Result<Box<dyn PreparedNativeRelay>, BootstrapError> {
        Err(BootstrapError::AuthorizationRefused)
    }
}

pub(crate) trait PreparedNativeRelay: Send {
    fn handle(&self) -> Arc<dyn crate::serve::Handle>;

    /// Release a relay whose complete fallible preparation finished before the wire ACK.
    fn commit(self: Box<Self>);
}

struct DisabledNativeBootstrapHandler;

impl NativeBootstrapHandler for DisabledNativeBootstrapHandler {
    fn verify_terminal(
        &self,
        _peer: PeerIdentity,
        _stdin: BorrowedFd<'_>,
        _stdout: BorrowedFd<'_>,
    ) -> Result<TerminalGeometryObservation, BootstrapError> {
        Err(BootstrapError::AuthorizationRefused)
    }

    fn authorized(
        &self,
        request: ConsumedNativeRequest<'_>,
        _deadline: &NativeLaunchDeadline,
    ) -> Result<PendingNativeLaunchReceipt, BootstrapError> {
        let registry = production_native_facades();
        let (_selected, _terminal) = select_consumed_direct_cli(&registry, request)
            .ok_or(BootstrapError::AuthorizationRefused)?;
        Err(BootstrapError::AuthorizationRefused)
    }
}

pub(crate) struct NativeBootstrapService {
    authority: CapabilityAuthority,
    handler: Arc<dyn NativeBootstrapHandler>,
    expected_project: PathBuf,
    supported_wire_version: u32,
    active_connections: Arc<AtomicUsize>,
    handshake_clock: Arc<dyn MonotonicClock>,
    handshake_timeout: Duration,
    verify_received_terminal: bool,
    #[cfg(test)]
    claim_ack_sender: Mutex<Option<ClaimAckSender>>,
    #[cfg(test)]
    claim_timeout_reset: Mutex<Option<ClaimTimeoutReset>>,
    #[cfg(test)]
    claimed_conn_writer_spawner: Mutex<Option<ClaimedConnWriterSpawner>>,
}

#[cfg(test)]
type ClaimAckSender =
    Box<dyn FnMut(&UnixStream, &[u8], SendFlags) -> rustix::io::Result<usize> + Send + 'static>;

#[cfg(test)]
type ClaimTimeoutReset = Box<dyn FnOnce(&UnixStream) -> std::io::Result<()> + Send + 'static>;

#[cfg(test)]
type ClaimedConnWriterSpawner = Box<
    dyn FnOnce(
            String,
            Box<dyn FnOnce() + Send + 'static>,
        ) -> std::io::Result<std::thread::JoinHandle<()>>
        + Send
        + 'static,
>;

#[derive(Debug)]
struct NativeLaunchOutcome {
    deadline: NativeLaunchDeadline,
    pending: Result<PendingNativeLaunchReceipt, BootstrapError>,
    claimant: NativeClaimant,
}

impl NativeBootstrapService {
    pub(crate) fn disabled(expected_project: PathBuf) -> Self {
        Self {
            authority: CapabilityAuthority::default(),
            handler: Arc::new(DisabledNativeBootstrapHandler),
            expected_project,
            supported_wire_version: NATIVE_WIRE_VERSION,
            active_connections: Arc::new(AtomicUsize::new(0)),
            handshake_clock: Arc::new(SystemClock::default()),
            handshake_timeout: HANDSHAKE_TIMEOUT,
            verify_received_terminal: true,
            #[cfg(test)]
            claim_ack_sender: Mutex::new(None),
            #[cfg(test)]
            claim_timeout_reset: Mutex::new(None),
            #[cfg(test)]
            claimed_conn_writer_spawner: Mutex::new(None),
        }
    }

    #[allow(dead_code, reason = "Task 3 installs the production terminal verifier")]
    pub(crate) fn new(
        expected_project: PathBuf,
        supported_wire_version: u32,
        handler: Arc<dyn NativeBootstrapHandler>,
    ) -> Self {
        Self {
            authority: CapabilityAuthority::default(),
            handler,
            expected_project,
            supported_wire_version,
            active_connections: Arc::new(AtomicUsize::new(0)),
            handshake_clock: Arc::new(SystemClock::default()),
            handshake_timeout: HANDSHAKE_TIMEOUT,
            verify_received_terminal: true,
            #[cfg(test)]
            claim_ack_sender: Mutex::new(None),
            #[cfg(test)]
            claim_timeout_reset: Mutex::new(None),
            #[cfg(test)]
            claimed_conn_writer_spawner: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_without_terminal_verification_for_task2_tests(
        expected_project: PathBuf,
        supported_wire_version: u32,
        handler: Arc<dyn NativeBootstrapHandler>,
    ) -> Self {
        Self {
            authority: CapabilityAuthority::default(),
            handler,
            expected_project,
            supported_wire_version,
            active_connections: Arc::new(AtomicUsize::new(0)),
            handshake_clock: Arc::new(SystemClock::default()),
            handshake_timeout: HANDSHAKE_TIMEOUT,
            verify_received_terminal: false,
            claim_ack_sender: Mutex::new(None),
            claim_timeout_reset: Mutex::new(None),
            claimed_conn_writer_spawner: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn with_handshake_timeout(
        expected_project: PathBuf,
        supported_wire_version: u32,
        handler: Arc<dyn NativeBootstrapHandler>,
        handshake_clock: Arc<dyn MonotonicClock>,
        handshake_timeout: Duration,
    ) -> Self {
        Self {
            authority: CapabilityAuthority::default(),
            handler,
            expected_project,
            supported_wire_version,
            active_connections: Arc::new(AtomicUsize::new(0)),
            handshake_clock,
            handshake_timeout,
            verify_received_terminal: true,
            claim_ack_sender: Mutex::new(None),
            claim_timeout_reset: Mutex::new(None),
            claimed_conn_writer_spawner: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn with_clock_without_terminal_verification_for_tests(
        expected_project: PathBuf,
        supported_wire_version: u32,
        handler: Arc<dyn NativeBootstrapHandler>,
        handshake_clock: Arc<dyn MonotonicClock>,
    ) -> Self {
        Self {
            authority: CapabilityAuthority::default(),
            handler,
            expected_project,
            supported_wire_version,
            active_connections: Arc::new(AtomicUsize::new(0)),
            handshake_clock,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            verify_received_terminal: false,
            claim_ack_sender: Mutex::new(None),
            claim_timeout_reset: Mutex::new(None),
            claimed_conn_writer_spawner: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn set_claim_ack_sender_for_tests(
        &self,
        sender: impl FnMut(&UnixStream, &[u8], SendFlags) -> rustix::io::Result<usize> + Send + 'static,
    ) {
        assert!(
            lock(&self.claim_ack_sender)
                .replace(Box::new(sender))
                .is_none()
        );
    }

    #[cfg(test)]
    fn set_claim_timeout_reset_for_tests(
        &self,
        reset: impl FnOnce(&UnixStream) -> std::io::Result<()> + Send + 'static,
    ) {
        assert!(
            lock(&self.claim_timeout_reset)
                .replace(Box::new(reset))
                .is_none()
        );
    }

    #[cfg(test)]
    fn set_claimed_conn_writer_spawner_for_tests(
        &self,
        spawner: impl FnOnce(
            String,
            Box<dyn FnOnce() + Send + 'static>,
        ) -> std::io::Result<std::thread::JoinHandle<()>>
        + Send
        + 'static,
    ) {
        assert!(
            lock(&self.claimed_conn_writer_spawner)
                .replace(Box::new(spawner))
                .is_none()
        );
    }

    fn clear_claim_timeouts(&self, stream: &UnixStream) -> std::io::Result<()> {
        #[cfg(test)]
        if let Some(reset) = lock(&self.claim_timeout_reset).take() {
            reset(stream)?;
        } else {
            stream.set_read_timeout(None)?;
            stream.set_write_timeout(None)?;
        }
        #[cfg(not(test))]
        {
            stream.set_read_timeout(None)?;
            stream.set_write_timeout(None)?;
        }
        #[cfg(target_os = "macos")]
        rustix::net::sockopt::set_socket_nosigpipe(stream, true).map_err(std::io::Error::from)?;
        Ok(())
    }

    fn prepare_claimed_conn(
        &self,
        id: ConnId,
        stream: &UnixStream,
        handle: Arc<dyn crate::serve::Handle>,
    ) -> Result<
        crate::serve::PreparedClaimedConn,
        (Arc<dyn crate::serve::Handle>, crate::serve::Departure),
    > {
        #[cfg(test)]
        if let Some(spawner) = lock(&self.claimed_conn_writer_spawner).take() {
            return crate::serve::PreparedClaimedConn::prepare_with_test_writer_spawner(
                id,
                stream,
                handle,
                crate::serve::OUTBOUND_FRAME_WRITE_TIMEOUT,
                spawner,
            );
        }
        crate::serve::PreparedClaimedConn::prepare(
            id,
            stream,
            handle,
            crate::serve::OUTBOUND_FRAME_WRITE_TIMEOUT,
        )
    }

    fn send_claim_ack_attempt(
        &self,
        stream: &UnixStream,
        flags: SendFlags,
    ) -> rustix::io::Result<usize> {
        #[cfg(test)]
        if let Some(sender) = lock(&self.claim_ack_sender).as_mut() {
            return sender(stream, &[WIRE_OK], flags);
        }
        send(stream, &[WIRE_OK], flags)
    }

    fn write_claim_ack(
        &self,
        stream: &UnixStream,
        deadline: &HandshakeDeadline,
    ) -> Result<(), BootstrapError> {
        // The claimed relay must inherit a blocking socket with no bootstrap deadlines, and
        // resetting either option after success would be a new fallible step behind WIRE_OK.
        // MSG_DONTWAIT bounds this one-byte acknowledgement without mutating the socket state the
        // relay inherits. A full buffer is a refusal; no lifecycle has started yet.
        #[allow(
            unused_mut,
            reason = "Linux adds MSG_NOSIGNAL; macOS uses SO_NOSIGPIPE"
        )]
        let mut flags = SendFlags::DONTWAIT;
        #[cfg(target_os = "linux")]
        {
            flags |= SendFlags::NOSIGNAL;
        }
        loop {
            deadline.require_remaining().map_err(|error| match error {
                BootstrapError::HandshakeExpired => BootstrapError::LaunchResultExpired,
                error => error,
            })?;
            match self.send_claim_ack_attempt(stream, flags) {
                Ok(1) => return Ok(()),
                Ok(_) => {
                    return Err(BootstrapError::NativeTransportIo(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "native claim acknowledgement was not written atomically",
                    )));
                }
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => {
                    return Err(BootstrapError::NativeTransportIo(error.into()));
                }
            }
        }
    }

    pub(crate) fn try_acquire_connection(&self) -> Result<ActiveNativeConnection, BootstrapError> {
        ActiveNativeConnection::acquire(&self.active_connections)
    }

    pub(crate) fn admit_connection(
        &self,
        stream: &mut UnixStream,
    ) -> Option<ActiveNativeConnection> {
        match self.try_acquire_connection() {
            Ok(active) => Some(active),
            Err(_) => {
                let _ = stream.write_all(&[WIRE_REFUSED]);
                let _ = stream.flush();
                None
            }
        }
    }

    pub(crate) fn serve_connection(
        &self,
        id: ConnId,
        stream: UnixStream,
        _active: ActiveNativeConnection,
    ) {
        let Ok(deadline) =
            HandshakeDeadline::new(Arc::clone(&self.handshake_clock), self.handshake_timeout)
        else {
            self.authority.revoke_connection(id);
            return;
        };
        let result = self.serve_authenticated(id, &stream, &deadline);
        match result {
            Ok(NativeLaunchOutcome {
                deadline: result_deadline,
                pending: Ok(pending),
                claimant,
            }) => {
                // Capability authentication has finished. Launch preparation and response delivery
                // deliberately start a fresh budget rather than inheriting whatever remained of
                // the descriptor handshake's ten seconds.
                let prepared = {
                    let mut transport = DeadlineIo::new(&stream, &result_deadline.0);
                    if write_native_launch_receipt(&mut transport, pending.receipt()).is_err() {
                        self.authority.revoke_connection(id);
                        return;
                    }
                    if !self.handler.requires_claim_transport() {
                        pending.commit();
                        self.authority.revoke_connection(id);
                        return;
                    }
                    let claimed = read_native_claim_request(&mut transport).and_then(|claim| {
                        self.handler
                            .prepare_native_claim(&claim.ticket, &claim.agent_id, claimant)
                    });
                    let Ok(prepared) = claimed else {
                        let _ = transport.write_all(&[WIRE_REFUSED]);
                        let _ = transport.flush();
                        self.authority.revoke_connection(id);
                        return;
                    };
                    prepared
                };
                if self.clear_claim_timeouts(&stream).is_err() {
                    let _ = (&stream).write_all(&[WIRE_REFUSED]);
                    let _ = (&stream).flush();
                    self.authority.revoke_connection(id);
                    return;
                }
                let relay = prepared.handle();
                let claimed = match self.prepare_claimed_conn(id, &stream, Arc::clone(&relay)) {
                    Ok(claimed) => claimed,
                    Err((_relay, _departure)) => {
                        let _ = send(&stream, &[WIRE_REFUSED], SendFlags::DONTWAIT);
                        self.authority.revoke_connection(id);
                        return;
                    }
                };
                let ack_deadline = match claim_ack_deadline(&result_deadline) {
                    Ok(deadline) => deadline,
                    Err(_) => {
                        let _ = send(&stream, &[WIRE_REFUSED], SendFlags::DONTWAIT);
                        self.authority.revoke_connection(id);
                        return;
                    }
                };
                if self.write_claim_ack(&stream, &ack_deadline).is_err() {
                    self.authority.revoke_connection(id);
                    return;
                }
                prepared.commit();
                pending.commit();
                self.authority.revoke_connection(id);
                drop(stream);
                let stopping = AtomicBool::new(false);
                claimed.run(&stopping);
                return;
            }
            Ok(NativeLaunchOutcome {
                deadline: result_deadline,
                pending: Err(_),
                claimant: _,
            }) => {
                let mut transport = DeadlineIo::new(&stream, &result_deadline.0);
                let _ = transport.write_all(&[WIRE_REFUSED]);
                let _ = transport.flush();
            }
            Err(_) => {
                let mut transport = DeadlineIo::new(&stream, &deadline);
                let _ = transport.write_all(&[WIRE_REFUSED]);
                let _ = transport.flush();
            }
        }
        self.authority.revoke_connection(id);
    }

    fn serve_authenticated(
        &self,
        id: ConnId,
        stream: &UnixStream,
        deadline: &HandshakeDeadline,
    ) -> Result<NativeLaunchOutcome, BootstrapError> {
        self.serve_authenticated_with_receiver(
            id,
            stream,
            deadline,
            receive_terminal_descriptors_before,
        )
    }

    fn serve_authenticated_with_receiver(
        &self,
        id: ConnId,
        stream: &UnixStream,
        deadline: &HandshakeDeadline,
        receive_descriptors: impl FnOnce(
            &UnixStream,
            &HandshakeDeadline,
        ) -> Result<[OwnedFd; 2], BootstrapError>,
    ) -> Result<NativeLaunchOutcome, BootstrapError> {
        let connection = NativeBootstrapConnection::authenticate(
            id,
            stream
                .try_clone()
                .map_err(BootstrapError::NativeTransportIo)?,
        )?;
        let [stdin, stdout] = receive_descriptors(&connection.stream, deadline)?;
        let mut transport = DeadlineIo::new(&connection.stream, deadline);
        let issue = read_wire_request(&mut transport)?.authenticate_issue()?;
        self.validate_context(&issue.context)?;
        deadline.require_remaining()?;
        let terminal = if self.verify_received_terminal {
            let witness = verify_bootstrap_tty(connection.peer, [stdin, stdout])
                .map_err(|error| BootstrapError::TerminalVerification(error.to_string()))?;
            let _handoff =
                self.handler
                    .verify_terminal(connection.peer, witness.stdin(), witness.stdout())?;
            bind_verified_terminal(connection.id, connection.peer, witness)
        } else {
            let handoff =
                self.handler
                    .verify_terminal(connection.peer, stdin.as_fd(), stdout.as_fd())?;
            bind_terminal_identity(
                connection.id,
                connection.peer,
                handoff.geometry,
                stdin,
                stdout,
            )?
        };
        #[cfg(test)]
        observe_native_route_test_stage(NativeRouteTestStage::DescriptorsVerified);
        let selector = context_selector(&issue.context)?;
        deadline.require_remaining()?;
        let capability = self
            .authority
            .issue(&connection, &terminal, selector, issue.hash)?;
        transport
            .write_all(&[WIRE_OK])
            .map_err(BootstrapError::NativeTransportIo)?;
        transport
            .write_all(&capability.wire_bytes())
            .map_err(BootstrapError::NativeTransportIo)?;
        transport
            .flush()
            .map_err(BootstrapError::NativeTransportIo)?;

        let consume = read_wire_request(&mut transport)?.authenticate_consume()?;
        self.validate_context(&consume.context)?;
        let selector = context_selector(&consume.context)?;
        deadline.require_remaining()?;
        terminal.revalidate_peer()?;
        let authorization = self.authority.consume(
            &connection,
            &terminal,
            selector,
            consume.hash,
            consume.token,
        )?;
        #[cfg(test)]
        observe_native_route_test_stage(NativeRouteTestStage::CapabilityConsumed);
        deadline.require_remaining()?;
        let launch_deadline =
            NativeLaunchDeadline::new(Arc::clone(&self.handshake_clock), LAUNCH_RESULT_TIMEOUT)?;
        let pending = self.handler.authorized(
            ConsumedNativeRequest {
                context: &consume.context,
                hash: consume.hash,
                terminal,
                authorization,
            },
            &launch_deadline,
        );
        Ok(NativeLaunchOutcome {
            deadline: launch_deadline,
            pending,
            claimant: NativeClaimant::from_authenticated_connection(connection.id, connection.peer),
        })
    }

    fn validate_context(&self, context: &DirectNativeRequestContext) -> Result<(), BootstrapError> {
        if context.canonical_project() != self.expected_project {
            return Err(BootstrapError::NativeWireProjectMismatch);
        }
        if context.native_wire_version() != self.supported_wire_version {
            return Err(BootstrapError::NativeWireVersionUnsupported);
        }
        Ok(())
    }

    #[allow(
        dead_code,
        reason = "security diagnostic used by transport conformance tests"
    )]
    pub(crate) fn capability_lookup_count(&self) -> u64 {
        self.authority.capability_lookup_count()
    }
}

pub(crate) struct ActiveNativeConnection {
    counter: Arc<AtomicUsize>,
}

impl ActiveNativeConnection {
    fn acquire(counter: &Arc<AtomicUsize>) -> Result<Self, BootstrapError> {
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_ACTIVE_NATIVE_CONNECTIONS).then_some(active + 1)
            })
            .map_err(|_| BootstrapError::NativeConnectionLimit)?;
        Ok(Self {
            counter: Arc::clone(counter),
        })
    }
}

impl Drop for ActiveNativeConnection {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

impl fmt::Debug for DirectCliCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DirectCliCapability([REDACTED])")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapabilityBinding {
    connection: ConnId,
    peer: PeerIdentity,
    terminal: TerminalFingerprint,
    selector: String,
    hash: ContextHash,
    geometry: TerminalGeometry,
    expires_at: Duration,
}

/// Server-side owner of native bootstrap capabilities.
pub(crate) struct CapabilityAuthority {
    rng: Arc<dyn CapabilityRng>,
    clock: Arc<dyn MonotonicClock>,
    capabilities: Mutex<HashMap<[u8; 32], CapabilityBinding>>,
    lookup_count: AtomicU64,
}

impl fmt::Debug for CapabilityAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapabilityAuthority")
            .field("capability_count", &lock(&self.capabilities).len())
            .finish_non_exhaustive()
    }
}

impl Default for CapabilityAuthority {
    fn default() -> Self {
        Self::with_sources(Arc::new(SystemRng), Arc::new(SystemClock::default()))
    }
}

impl CapabilityAuthority {
    pub(crate) fn with_sources(
        rng: Arc<dyn CapabilityRng>,
        clock: Arc<dyn MonotonicClock>,
    ) -> Self {
        Self {
            rng,
            clock,
            capabilities: Mutex::new(HashMap::new()),
            lookup_count: AtomicU64::new(0),
        }
    }

    pub(crate) fn issue(
        &self,
        connection: &NativeBootstrapConnection,
        terminal: &BoundTerminalDescriptors,
        selector: &str,
        hash: ContextHash,
    ) -> Result<DirectCliCapability, BootstrapError> {
        if terminal.connection != connection.id || terminal.peer != connection.peer {
            return Err(BootstrapError::CapabilityBinding);
        }
        let mut capabilities = lock(&self.capabilities);
        let now = self.clock.now();
        let expires_at = now
            .checked_add(CAPABILITY_TTL)
            .ok_or(BootstrapError::ClockOverflow)?;
        capabilities.retain(|_, binding| now < binding.expires_at);
        for _ in 0..MAX_CAPABILITY_GENERATION_ATTEMPTS {
            let mut token = [0u8; 32];
            self.rng.fill(&mut token)?;
            if token == [0; 32] || capabilities.contains_key(&token) {
                continue;
            }
            capabilities.insert(
                token,
                CapabilityBinding {
                    connection: connection.id,
                    peer: connection.peer,
                    terminal: terminal.fingerprint,
                    selector: selector.to_owned(),
                    hash,
                    geometry: terminal.geometry,
                    expires_at,
                },
            );
            return Ok(DirectCliCapability(token));
        }
        Err(BootstrapError::CapabilityEntropyExhausted)
    }

    /// Atomically validate every binding and consume the token on success.
    pub(crate) fn consume<'selector>(
        &self,
        connection: &NativeBootstrapConnection,
        terminal: &BoundTerminalDescriptors,
        selector: &'selector str,
        hash: ContextHash,
        token: [u8; 32],
    ) -> Result<AuthorizedNativeFacade<'selector>, BootstrapError> {
        self.lookup_count.fetch_add(1, Ordering::Relaxed);
        let mut capabilities = lock(&self.capabilities);
        let binding = capabilities
            .get(&token)
            .ok_or(BootstrapError::CapabilityUnknownOrUsed)?;
        if self.clock.now() >= binding.expires_at {
            capabilities.remove(&token);
            return Err(BootstrapError::CapabilityExpired);
        }
        if binding.connection != connection.id
            || binding.peer != connection.peer
            || terminal.connection != connection.id
            || terminal.peer != connection.peer
            || binding.terminal != terminal.fingerprint
            || binding.geometry != terminal.geometry
            || binding.selector != selector
            || binding.hash != hash
        {
            return Err(BootstrapError::CapabilityBinding);
        }
        capabilities.remove(&token);
        Ok(AuthorizedNativeFacade::from_consumed(
            ConsumedNativeCapability::new(selector),
        ))
    }

    /// Aggregate diagnostic used to prove an ordinary transport refusal preceded token lookup.
    pub(crate) fn capability_lookup_count(&self) -> u64 {
        self.lookup_count.load(Ordering::Relaxed)
    }

    fn revoke_connection(&self, connection: ConnId) {
        lock(&self.capabilities).retain(|_, binding| binding.connection != connection);
    }
}

/// A native bootstrap transport or authorization refusal.
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("native bootstrap peer credentials are unavailable: {0}")]
    PeerCredentials(std::io::Error),
    #[error("native bootstrap peer uid does not match the supervisor uid")]
    PeerUidMismatch,
    #[error("native bootstrap transport I/O failed: {0}")]
    NativeTransportIo(std::io::Error),
    #[error("native bootstrap descriptor message was malformed")]
    DescriptorMessage,
    #[error("native bootstrap requires exactly stdin and stdout descriptors")]
    DescriptorCount,
    #[error("native bootstrap ancillary descriptor data was truncated")]
    AncillaryTruncated,
    #[error("native bootstrap requires atomic close-on-exec descriptor receipt on this platform")]
    AtomicCloexecUnsupported,
    #[error("native bootstrap descriptors name different terminals")]
    DifferentTerminals,
    #[error("native bootstrap stdin/stdout descriptors were swapped or have invalid access")]
    DescriptorRoles,
    #[error("native capability entropy failed: {0}")]
    Random(String),
    #[error("native capability entropy did not produce a unique token within the retry limit")]
    CapabilityEntropyExhausted,
    #[error("native capability clock overflowed")]
    ClockOverflow,
    #[error("native capability is bound to different authenticated state")]
    CapabilityBinding,
    #[error("native capability is unknown or was already consumed")]
    CapabilityUnknownOrUsed,
    #[error("native capability expired")]
    CapabilityExpired,
    #[error("native bootstrap wire protocol is invalid")]
    NativeWireProtocol,
    #[error("native bootstrap canonical project does not match this supervisor")]
    NativeWireProjectMismatch,
    #[error("native bootstrap wire version is not supported by this supervisor")]
    NativeWireVersionUnsupported,
    #[error("native bootstrap request context exceeds the aggregate wire budget")]
    NativeWireContextTooLarge,
    #[error("native bootstrap context hash did not match the received canonical request")]
    NativeWireContextHashMismatch,
    #[error("native bootstrap has reached its active connection limit")]
    NativeConnectionLimit,
    #[error("native bootstrap handshake deadline expired")]
    HandshakeExpired,
    #[error("native launch result deadline expired")]
    LaunchResultExpired,
    #[error("native bootstrap authorization was refused")]
    AuthorizationRefused,
    #[error("native command could not be prepared: {0}")]
    NativeCommand(String),
    #[error("native bootstrap terminal verification failed: {0}")]
    TerminalVerification(String),
    #[error("native pane claim failed: {0}")]
    NativeClaim(String),
}

fn io_error(error: rustix::io::Errno) -> BootstrapError {
    BootstrapError::NativeTransportIo(error.into())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(target_os = "linux")]
fn peer_identity(fd: RawFd) -> Result<PeerIdentity, BootstrapError> {
    // SAFETY: `fd` belongs to the live `UnixStream` held by the caller.
    let fd = unsafe { BorrowedFd::borrow_raw(fd) };
    let credentials = rustix::net::sockopt::socket_peercred(fd)
        .map_err(|error| BootstrapError::PeerCredentials(error.into()))?;
    Ok(PeerIdentity {
        uid: credentials.uid.as_raw(),
        pid: credentials.pid.as_raw_nonzero().get() as u32,
    })
}

#[cfg(target_os = "macos")]
fn peer_identity(fd: RawFd) -> Result<PeerIdentity, BootstrapError> {
    use std::ffi::{c_int, c_void};

    unsafe extern "C" {
        fn getpeereid(fd: c_int, uid: *mut u32, gid: *mut u32) -> c_int;
        fn getsockopt(
            socket: c_int,
            level: c_int,
            name: c_int,
            value: *mut c_void,
            value_len: *mut u32,
        ) -> c_int;
    }

    let (mut uid, mut gid) = (0, 0);
    // SAFETY: both pointers name initialized writable locals and `fd` is a live Unix socket.
    if unsafe { getpeereid(fd, &mut uid, &mut gid) } != 0 {
        return Err(BootstrapError::PeerCredentials(
            std::io::Error::last_os_error(),
        ));
    }

    const SOL_LOCAL: c_int = 0;
    const LOCAL_PEERPID: c_int = 2;
    let mut pid: c_int = 0;
    let mut len = std::mem::size_of::<c_int>() as u32;
    // SAFETY: `pid` is valid for `len` bytes and the socket stays open for the call.
    if unsafe {
        getsockopt(
            fd,
            SOL_LOCAL,
            LOCAL_PEERPID,
            (&raw mut pid).cast::<c_void>(),
            &mut len,
        )
    } != 0
        || len as usize != std::mem::size_of::<c_int>()
        || pid <= 0
    {
        return Err(BootstrapError::PeerCredentials(
            std::io::Error::last_os_error(),
        ));
    }

    Ok(PeerIdentity {
        uid,
        pid: pid as u32,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn peer_identity(_fd: RawFd) -> Result<PeerIdentity, BootstrapError> {
    Err(BootstrapError::PeerCredentials(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "native bootstrap requires peer uid and pid credentials",
    )))
}

#[cfg(test)]
#[path = "native_bootstrap/tests.rs"]
mod tests;
