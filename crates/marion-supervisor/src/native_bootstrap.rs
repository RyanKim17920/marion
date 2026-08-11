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
#[cfg(target_os = "linux")]
use std::io::IoSliceMut;
use std::io::{IoSlice, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rustix::fs::{OFlags, fcntl_getfl, fstat};
#[cfg(target_os = "linux")]
use rustix::io::{FdFlags, fcntl_setfd};
#[cfg(target_os = "linux")]
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg};
use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};

use crate::native_intent::AuthorizedNativeFacade;
use crate::serve::{ConnId, own_uid};

const CONTEXT_DOMAIN: &[u8] = b"marion/direct-native-context/v1\0";
const DESCRIPTOR_MESSAGE: &[u8] = b"MNB1";
const CAPABILITY_TTL: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ACTIVE_NATIVE_CONNECTIONS: usize = 32;
const MAX_CONTEXT_FIELD_BYTES: usize = 64 * 1024;
const MAX_CONTEXT_ARGUMENTS: usize = 4096;
const MAX_CONTEXT_BYTES: usize = 192 * 1024;
const MAX_SELECTOR_BYTES: usize = 255;
const MAX_CAPABILITY_GENERATION_ATTEMPTS: usize = 128;
pub(crate) const NATIVE_WIRE_VERSION: u32 = 1;

/// The exact secret-free request state authenticated by the native bootstrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectNativeRequestContext {
    canonical_project: PathBuf,
    selector: OsString,
    opaque_tail: Vec<OsString>,
    terminal_profile: OsString,
    native_wire_version: u32,
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
        }
    }

    pub(crate) fn canonical_project(&self) -> &std::path::Path {
        &self.canonical_project
    }

    pub(crate) const fn native_wire_version(&self) -> u32 {
        self.native_wire_version
    }
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

    #[cfg(test)]
    pub const fn pid(self) -> u32 {
        self.pid
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
struct TerminalFingerprint {
    device: u64,
    inode: u64,
    special_device: u64,
}

/// A received stdin/stdout pair bound to one identity and a later terminal observation.
#[derive(Debug)]
pub(crate) struct BoundTerminalDescriptors {
    connection: ConnId,
    peer: PeerIdentity,
    fingerprint: TerminalFingerprint,
    geometry: TerminalGeometry,
    _stdin: OwnedFd,
    _stdout: OwnedFd,
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
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (stream, deadline);
        Err(BootstrapError::AtomicCloexecUnsupported)
    }

    #[cfg(target_os = "linux")]
    receive_terminal_descriptors_atomic(stream, deadline)
}

#[cfg(target_os = "linux")]
fn receive_terminal_descriptors_atomic(
    stream: &UnixStream,
    deadline: &HandshakeDeadline,
) -> Result<[OwnedFd; 2], BootstrapError> {
    deadline.install_read_timeout(stream)?;
    let mut message = [0u8; DESCRIPTOR_MESSAGE.len()];
    let mut iov = [IoSliceMut::new(&mut message)];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(3))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let receive_flags = RecvFlags::CMSG_CLOEXEC;
    let received = recvmsg(stream, &mut iov, &mut control, receive_flags).map_err(io_error)?;
    let mut descriptors = Vec::new();
    for ancillary in control.drain() {
        if let RecvAncillaryMessage::ScmRights(rights) = ancillary {
            descriptors.extend(rights);
        }
    }
    if received.flags.contains(rustix::net::ReturnFlags::CTRUNC) {
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

#[cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "non-Linux refuses before descriptor marker receipt"
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
        _stdin: stdin,
        _stdout: stdout,
    })
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

    #[cfg(target_os = "linux")]
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
) -> Result<(), BootstrapError> {
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
    if status[0] == WIRE_OK {
        Ok(())
    } else {
        Err(BootstrapError::AuthorizationRefused)
    }
}

fn write_wire_request(
    stream: &mut UnixStream,
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

fn write_context(
    stream: &mut UnixStream,
    context: &DirectNativeRequestContext,
) -> Result<(), BootstrapError> {
    fn field(stream: &mut UnixStream, value: &OsStr) -> Result<(), BootstrapError> {
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
        .map_err(BootstrapError::NativeTransportIo)
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
    Ok(DirectNativeRequestContext {
        canonical_project,
        selector,
        opaque_tail,
        terminal_profile,
        native_wire_version: u32::from_be_bytes(native_wire_version),
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
) -> Result<usize, BootstrapError> {
    if argument_count > MAX_CONTEXT_ARGUMENTS {
        return Err(BootstrapError::NativeWireProtocol);
    }
    let mut size = 8usize;
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
            .chain(std::iter::once(context.terminal_profile.as_bytes().len())),
        context.opaque_tail.len(),
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

    fn authorized(&self, request: ConsumedNativeRequest<'_>) -> Result<(), BootstrapError>;
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

    fn authorized(&self, _request: ConsumedNativeRequest<'_>) -> Result<(), BootstrapError> {
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
        let status = if result.is_ok() {
            WIRE_OK
        } else {
            WIRE_REFUSED
        };
        let mut transport = DeadlineIo::new(&stream, &deadline);
        let _ = transport.write_all(&[status]);
        let _ = transport.flush();
        self.authority.revoke_connection(id);
    }

    fn serve_authenticated(
        &self,
        id: ConnId,
        stream: &UnixStream,
        deadline: &HandshakeDeadline,
    ) -> Result<(), BootstrapError> {
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
    ) -> Result<(), BootstrapError> {
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
        let handoff =
            self.handler
                .verify_terminal(connection.peer, stdin.as_fd(), stdout.as_fd())?;
        let terminal = bind_terminal_identity(
            connection.id,
            connection.peer,
            handoff.geometry,
            stdin,
            stdout,
        )?;
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
        let authorization = self.authority.consume(
            &connection,
            &terminal,
            selector,
            consume.hash,
            consume.token,
        )?;
        deadline.require_remaining()?;
        self.handler.authorized(ConsumedNativeRequest {
            context: &consume.context,
            hash: consume.hash,
            terminal,
            authorization,
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
    #[error("native bootstrap authorization was refused")]
    AuthorizationRefused,
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
