use super::fakes::{ManualClock, SequenceRng};
use super::*;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::OsStringExt;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::process::CommandExt;
use std::sync::Barrier;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use marion_core::{
    Lane, NativeAdapterId, NativeFacadeDescriptor, NativeFacadeRegistry, NativeLane, VendorIdentity,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rustix::process::{Pid, getpgrp, getsid};

#[derive(Default)]
struct Effects {
    descriptor_details: AtomicU64,
    filesystem: AtomicU64,
    processes: AtomicU64,
    artifacts: AtomicU64,
}

impl Effects {
    fn assert_zero(&self) {
        assert_eq!(self.descriptor_details.load(Ordering::SeqCst), 0);
        assert_eq!(self.filesystem.load(Ordering::SeqCst), 0);
        assert_eq!(self.processes.load(Ordering::SeqCst), 0);
        assert_eq!(self.artifacts.load(Ordering::SeqCst), 0);
    }

    fn record_authorized(&self, _authorization: &AuthorizedNativeFacade<'_>) {
        self.descriptor_details.fetch_add(1, Ordering::SeqCst);
        self.filesystem.fetch_add(1, Ordering::SeqCst);
        self.processes.fetch_add(1, Ordering::SeqCst);
        self.artifacts.fetch_add(1, Ordering::SeqCst);
    }
}

struct Fixture {
    _client: UnixStream,
    connection: NativeBootstrapConnection,
    terminal: BoundTerminalDescriptors,
}

fn geometry() -> TerminalGeometry {
    TerminalGeometry {
        cols: 101,
        rows: 37,
        xpixel: 3,
        ypixel: 5,
    }
}

fn fixture(id: u64) -> Fixture {
    let (client, server) = UnixStream::pair().unwrap();
    let connection = NativeBootstrapConnection::authenticate(ConnId(id), server).unwrap();
    let stdin = rustix::fs::open(
        "/dev/null",
        OFlags::RDONLY | OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .unwrap();
    let stdout = rustix::fs::open(
        "/dev/null",
        OFlags::WRONLY | OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .unwrap();
    let stat = fstat(&stdin).unwrap();
    let terminal = BoundTerminalDescriptors {
        connection: connection.id,
        peer: connection.peer,
        fingerprint: TerminalFingerprint {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
            special_device: stat.st_rdev as u64,
        },
        geometry: geometry(),
        witness: None,
        _test_stdin: Some(stdin),
        _test_stdout: Some(stdout),
    };
    Fixture {
        _client: client,
        connection,
        terminal,
    }
}

fn authority(clock: Arc<ManualClock>) -> CapabilityAuthority {
    CapabilityAuthority::with_sources(Arc::new(SequenceRng::default()), clock)
}

fn pending_receipt(agent_id: &str) -> PendingNativeLaunchReceipt {
    PendingNativeLaunchReceipt::new(
        NativeLaunchReceipt::new(
            AgentId(agent_id.into()),
            NativeLaunchTicket::for_test([0x5a; 32]),
        ),
        || {},
    )
}

const NATIVE_TEST_AGENT: &str = "019f0000-0000-7000-8000-000000000001";

fn launch_binding(peer: PeerIdentity) -> NativeLaunchBinding {
    NativeLaunchBinding::new(
        AgentId(NATIVE_TEST_AGENT.into()),
        PathBuf::from("/project"),
        peer,
        ConnId(12),
        TerminalFingerprint {
            device: 3,
            inode: 4,
            special_device: 5,
        },
        geometry(),
        NativeLaunchDescriptor::new("atlas", "atlas", "atlas-native"),
        context_hash(&request_context()),
    )
}

fn claimant(conn: ConnId, peer: PeerIdentity) -> NativeClaimant {
    NativeClaimant::new(conn, peer)
}

#[test]
fn native_claim_wire_presents_the_exact_agent_and_opaque_ticket() {
    let receipt = NativeLaunchReceipt::new(
        AgentId(NATIVE_TEST_AGENT.into()),
        NativeLaunchTicket::for_test([0x80; 32]),
    );
    let mut wire = Vec::new();
    write_native_claim_request(&mut wire, &receipt).unwrap();
    assert_eq!(&wire[..4], b"MNC1");
    let claim = read_native_claim_request(&mut wire.as_slice()).unwrap();
    assert_eq!(claim.agent_id, *receipt.agent_id());
    assert_eq!(claim.ticket, NativeLaunchTicket::for_test([0x80; 32]));

    let mut wrong_magic = wire.clone();
    wrong_magic[0] = b'X';
    assert!(matches!(
        read_native_claim_request(&mut wrong_magic.as_slice()),
        Err(BootstrapError::NativeWireProtocol)
    ));
}

/// Mutation: reset client socket timeouts after emitting the native claim. The server can consume
/// and prepare that claim before the local reset fails, leaving a success race the client cannot
/// safely enter. Reset failure must therefore produce zero claim bytes.
#[test]
fn client_timeout_reset_failure_sends_no_native_claim() {
    let receipt = NativeLaunchReceipt::new(
        AgentId(NATIVE_TEST_AGENT.into()),
        NativeLaunchTicket::for_test([0x81; 32]),
    );
    let (mut client, mut server) = UnixStream::pair().unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let observed = std::thread::spawn(move || {
        let claim = read_native_claim_request(&mut server).is_ok();
        if claim {
            server.write_all(&[WIRE_OK]).unwrap();
        }
        claim
    });

    let result = exchange_native_claim(&mut client, &receipt, |_| {
        Err(std::io::Error::other(
            "injected client timeout reset failure",
        ))
    });
    drop(client);
    assert!(matches!(result, Err(BootstrapError::NativeTransportIo(_))));
    assert!(
        !observed.join().unwrap(),
        "server received claim bytes before client timeout reset completed"
    );
}

#[test]
fn claim_ack_deadline_is_bounded_by_the_absolute_launch_deadline() {
    let clock = Arc::new(ManualClock::default());
    let deadline = NativeLaunchDeadline::new(
        Arc::clone(&clock) as Arc<dyn MonotonicClock>,
        LAUNCH_RESULT_TIMEOUT,
    )
    .unwrap();
    assert_eq!(
        claim_ack_deadline(&deadline)
            .unwrap()
            .require_remaining()
            .unwrap(),
        CLAIM_ACK_TIMEOUT
    );
    clock.set(LAUNCH_RESULT_TIMEOUT - Duration::from_millis(125));
    assert_eq!(
        claim_ack_deadline(&deadline)
            .unwrap()
            .require_remaining()
            .unwrap(),
        Duration::from_millis(125)
    );
    clock.set(LAUNCH_RESULT_TIMEOUT);
    assert!(matches!(
        claim_ack_deadline(&deadline),
        Err(BootstrapError::LaunchResultExpired)
    ));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Default)]
struct ClaimLifecycleHandle {
    connected: Mutex<Vec<ConnId>>,
    gone: Mutex<Vec<ConnId>>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl crate::serve::Handle for ClaimLifecycleHandle {
    fn connected(&self, conn: ConnId) {
        lock(&self.connected).push(conn);
    }

    fn call(
        &self,
        _conn: ConnId,
        call: &marion_proto::Call,
        _out: &crate::serve::Outbound,
    ) -> Result<marion_proto::MethodResult, marion_proto::RpcError> {
        Err(marion_proto::RpcError::unimplemented(
            call.method().as_str(),
            "claim lifecycle fixture accepts no calls",
            "native claim test",
        ))
    }

    fn gone(&self, conn: ConnId, _gone: &marion_proto::ClientGone, _why: &crate::serve::Departure) {
        lock(&self.gone).push(conn);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct ClaimLifecyclePrepared {
    handle: Arc<ClaimLifecycleHandle>,
    commits: Arc<AtomicU64>,
    aborts: Arc<AtomicU64>,
    committed: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl PreparedNativeRelay for ClaimLifecyclePrepared {
    fn handle(&self) -> Arc<dyn crate::serve::Handle> {
        self.handle.clone()
    }

    fn commit(mut self: Box<Self>) {
        self.committed = true;
        self.commits.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for ClaimLifecyclePrepared {
    fn drop(&mut self) {
        if !self.committed {
            self.aborts.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct ClaimLifecycleHandler {
    claimant: Mutex<Option<NativeClaimant>>,
    handle: Arc<ClaimLifecycleHandle>,
    commits: Arc<AtomicU64>,
    aborts: Arc<AtomicU64>,
    fail_preparations: Arc<AtomicU64>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl NativeBootstrapHandler for ClaimLifecycleHandler {
    fn verify_terminal(
        &self,
        _peer: PeerIdentity,
        _stdin: BorrowedFd<'_>,
        _stdout: BorrowedFd<'_>,
    ) -> Result<TerminalGeometryObservation, BootstrapError> {
        Ok(TerminalGeometryObservation::new(geometry()))
    }

    fn authorized(
        &self,
        _request: ConsumedNativeRequest<'_>,
        _deadline: &NativeLaunchDeadline,
    ) -> Result<PendingNativeLaunchReceipt, BootstrapError> {
        Ok(pending_receipt(NATIVE_TEST_AGENT))
    }

    fn requires_claim_transport(&self) -> bool {
        true
    }

    fn prepare_native_claim(
        &self,
        ticket: &NativeLaunchTicket,
        agent_id: &AgentId,
        claimant: NativeClaimant,
    ) -> Result<Box<dyn PreparedNativeRelay>, BootstrapError> {
        assert_eq!(agent_id, &AgentId(NATIVE_TEST_AGENT.into()));
        assert_eq!(ticket, &NativeLaunchTicket::for_test([0x5a; 32]));
        if self
            .fail_preparations
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(BootstrapError::NativeClaim(
                "injected lifecycle thread spawn failure".into(),
            ));
        }
        *lock(&self.claimant) = Some(claimant);
        Ok(Box::new(ClaimLifecyclePrepared {
            handle: Arc::clone(&self.handle),
            commits: Arc::clone(&self.commits),
            aborts: Arc::clone(&self.aborts),
            committed: false,
        }))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn start_claim_lifecycle_roundtrip(
    service: &Arc<NativeBootstrapService>,
    context: &DirectNativeRequestContext,
    conn: ConnId,
) -> (UnixStream, std::thread::JoinHandle<()>) {
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let worker = {
        let service = Arc::clone(service);
        let active = service.try_acquire_connection().unwrap();
        std::thread::spawn(move || service.serve_connection(conn, server, active))
    };
    let input = std::fs::File::open("/dev/null").unwrap();
    let output = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .unwrap();
    let capability =
        request_direct_cli_capability(&mut client, input.as_fd(), output.as_fd(), context).unwrap();
    let receipt = present_direct_cli_capability(&mut client, capability, context).unwrap();
    write_native_claim_request(&mut client, &receipt).unwrap();
    (client, worker)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn claim_lifecycle_roundtrip(
    service: &Arc<NativeBootstrapService>,
    context: &DirectNativeRequestContext,
    conn: ConnId,
) -> u8 {
    let (mut client, worker) = start_claim_lifecycle_roundtrip(service, context, conn);
    let mut status = [0xff];
    client.read_exact(&mut status).unwrap();
    drop(client);
    worker.join().unwrap();
    status[0]
}

/// Mutation: leave lifecycle-thread creation in the post-acknowledgement `commit`. The first
/// round trip then observes `WIRE_OK` even though the injected spawn refusal means no relay can
/// ever own that socket. A second authenticated claim proves the failure did not start a lifecycle
/// or poison preparation for a retry.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn lifecycle_thread_spawn_failure_precedes_claim_ack_and_keeps_retry_available() {
    let handle = Arc::new(ClaimLifecycleHandle::default());
    let commits = Arc::new(AtomicU64::new(0));
    let aborts = Arc::new(AtomicU64::new(0));
    let handler = Arc::new(ClaimLifecycleHandler {
        claimant: Mutex::new(None),
        handle: Arc::clone(&handle),
        commits: Arc::clone(&commits),
        aborts: Arc::clone(&aborts),
        fail_preparations: Arc::new(AtomicU64::new(1)),
    });
    let context = request_context();
    let service = Arc::new(
        NativeBootstrapService::new_without_terminal_verification_for_task2_tests(
            context.canonical_project().to_path_buf(),
            context.native_wire_version(),
            handler as Arc<dyn NativeBootstrapHandler>,
        ),
    );

    assert_eq!(
        claim_lifecycle_roundtrip(&service, &context, ConnId(4_211)),
        WIRE_REFUSED,
        "a lifecycle spawn failure was acknowledged as a usable relay"
    );
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert!(lock(&handle.connected).is_empty());
    assert!(lock(&handle.gone).is_empty());

    assert_eq!(
        claim_lifecycle_roundtrip(&service, &context, ConnId(4_212)),
        WIRE_OK,
        "the failed preparation poisoned the next authenticated claim"
    );
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert_eq!(aborts.load(Ordering::SeqCst), 0);
}

/// Mutation: release the prepared relay before the acknowledgement flush completes. The commit
/// counter would become observable while the injected flush remains blocked.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn blocked_claim_ack_flush_does_not_start_the_prepared_relay() {
    let handle = Arc::new(ClaimLifecycleHandle::default());
    let commits = Arc::new(AtomicU64::new(0));
    let aborts = Arc::new(AtomicU64::new(0));
    let handler = Arc::new(ClaimLifecycleHandler {
        claimant: Mutex::new(None),
        handle: Arc::clone(&handle),
        commits: Arc::clone(&commits),
        aborts: Arc::clone(&aborts),
        fail_preparations: Arc::new(AtomicU64::new(0)),
    });
    let context = request_context();
    let service = Arc::new(
        NativeBootstrapService::new_without_terminal_verification_for_task2_tests(
            context.canonical_project().to_path_buf(),
            context.native_wire_version(),
            handler as Arc<dyn NativeBootstrapHandler>,
        ),
    );
    let (at_flush_tx, at_flush_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    service.set_claim_ack_sender_for_tests(move |stream, bytes, flags| {
        let written = send(stream, bytes, flags)?;
        at_flush_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        Ok(written)
    });

    let (mut client, worker) = start_claim_lifecycle_roundtrip(&service, &context, ConnId(4_213));
    at_flush_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("claim acknowledgement reached the blocked flush");
    assert_eq!(
        commits.load(Ordering::SeqCst),
        0,
        "prepared relay started before claim acknowledgement flush"
    );
    release_tx.send(()).unwrap();
    let mut status = [0xff];
    client.read_exact(&mut status).unwrap();
    assert_eq!(status, [WIRE_OK]);
    drop(client);
    worker.join().unwrap();
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert_eq!(aborts.load(Ordering::SeqCst), 0);
}

/// Mutation: return a transport failure on the first interrupted ACK send instead of retrying the
/// same one-byte decisive write against its absolute deadline.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn interrupted_claim_ack_send_retries_before_relay_start() {
    let handle = Arc::new(ClaimLifecycleHandle::default());
    let commits = Arc::new(AtomicU64::new(0));
    let aborts = Arc::new(AtomicU64::new(0));
    let handler = Arc::new(ClaimLifecycleHandler {
        claimant: Mutex::new(None),
        handle: Arc::clone(&handle),
        commits: Arc::clone(&commits),
        aborts: Arc::clone(&aborts),
        fail_preparations: Arc::new(AtomicU64::new(0)),
    });
    let context = request_context();
    let service = Arc::new(
        NativeBootstrapService::new_without_terminal_verification_for_task2_tests(
            context.canonical_project().to_path_buf(),
            context.native_wire_version(),
            handler as Arc<dyn NativeBootstrapHandler>,
        ),
    );
    let attempts = Arc::new(AtomicU64::new(0));
    let observed_attempts = Arc::clone(&attempts);
    service.set_claim_ack_sender_for_tests(move |stream, bytes, flags| {
        if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(rustix::io::Errno::INTR)
        } else {
            send(stream, bytes, flags)
        }
    });

    assert_eq!(
        claim_lifecycle_roundtrip(&service, &context, ConnId(4_214)),
        WIRE_OK
    );
    assert_eq!(observed_attempts.load(Ordering::SeqCst), 2);
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert_eq!(aborts.load(Ordering::SeqCst), 0);
}

/// Mutation: forget to drop the prepared relay when acknowledgement output fails. That leaks its
/// pre-spawned worker and request-owned lifecycle instead of synchronously aborting preparation.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn claim_ack_write_failure_aborts_prepared_relay_without_starting_or_connecting() {
    let handle = Arc::new(ClaimLifecycleHandle::default());
    let commits = Arc::new(AtomicU64::new(0));
    let aborts = Arc::new(AtomicU64::new(0));
    let handler = Arc::new(ClaimLifecycleHandler {
        claimant: Mutex::new(None),
        handle: Arc::clone(&handle),
        commits: Arc::clone(&commits),
        aborts: Arc::clone(&aborts),
        fail_preparations: Arc::new(AtomicU64::new(0)),
    });
    let context = request_context();
    let service = Arc::new(
        NativeBootstrapService::new_without_terminal_verification_for_task2_tests(
            context.canonical_project().to_path_buf(),
            context.native_wire_version(),
            handler as Arc<dyn NativeBootstrapHandler>,
        ),
    );
    service.set_claim_ack_sender_for_tests(|_, _, _| Err(rustix::io::Errno::PIPE));

    let (client, worker) = start_claim_lifecycle_roundtrip(&service, &context, ConnId(4_214));
    worker.join().unwrap();
    drop(client);
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert_eq!(aborts.load(Ordering::SeqCst), 1);
    assert!(lock(&handle.connected).is_empty());
    assert!(lock(&handle.gone).is_empty());
}

/// Mutation: clear bootstrap socket deadlines after emitting `WIRE_OK`. An injected reset failure
/// then exposes success for a relay the service immediately kills instead of refusing before
/// release; a later clean round trip also proves failure did not poison preparation globally.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn claim_timeout_reset_failure_precedes_ack_and_relay_start() {
    let handle = Arc::new(ClaimLifecycleHandle::default());
    let commits = Arc::new(AtomicU64::new(0));
    let aborts = Arc::new(AtomicU64::new(0));
    let handler = Arc::new(ClaimLifecycleHandler {
        claimant: Mutex::new(None),
        handle: Arc::clone(&handle),
        commits: Arc::clone(&commits),
        aborts: Arc::clone(&aborts),
        fail_preparations: Arc::new(AtomicU64::new(0)),
    });
    let context = request_context();
    let service = Arc::new(
        NativeBootstrapService::new_without_terminal_verification_for_task2_tests(
            context.canonical_project().to_path_buf(),
            context.native_wire_version(),
            handler as Arc<dyn NativeBootstrapHandler>,
        ),
    );
    service.set_claim_timeout_reset_for_tests(|_| {
        Err(std::io::Error::other(
            "injected claim socket timeout reset failure",
        ))
    });

    assert_eq!(
        claim_lifecycle_roundtrip(&service, &context, ConnId(4_215)),
        WIRE_REFUSED,
        "timeout reset failure escaped behind WIRE_OK"
    );
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert_eq!(aborts.load(Ordering::SeqCst), 1);
    assert!(lock(&handle.connected).is_empty());

    assert_eq!(
        claim_lifecycle_roundtrip(&service, &context, ConnId(4_216)),
        WIRE_OK,
        "timeout reset failure poisoned the next claim preparation"
    );
    assert_eq!(commits.load(Ordering::SeqCst), 1);
}

/// Mutation: retain socket splitting or relay-worker creation after `WIRE_OK`. This injected
/// preflight failure then exposes success even though the prepared relay is immediately aborted.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn claimed_connection_preflight_failure_precedes_ack_and_relay_start() {
    let handle = Arc::new(ClaimLifecycleHandle::default());
    let commits = Arc::new(AtomicU64::new(0));
    let aborts = Arc::new(AtomicU64::new(0));
    let handler = Arc::new(ClaimLifecycleHandler {
        claimant: Mutex::new(None),
        handle: Arc::clone(&handle),
        commits: Arc::clone(&commits),
        aborts: Arc::clone(&aborts),
        fail_preparations: Arc::new(AtomicU64::new(0)),
    });
    let context = request_context();
    let service = Arc::new(
        NativeBootstrapService::new_without_terminal_verification_for_task2_tests(
            context.canonical_project().to_path_buf(),
            context.native_wire_version(),
            handler as Arc<dyn NativeBootstrapHandler>,
        ),
    );
    service.set_claimed_conn_writer_spawner_for_tests(|_, _| {
        Err(std::io::Error::other(
            "injected claimed-connection writer spawn failure",
        ))
    });

    assert_eq!(
        claim_lifecycle_roundtrip(&service, &context, ConnId(4_217)),
        WIRE_REFUSED,
        "claimed-connection preflight failure escaped behind WIRE_OK"
    );
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert_eq!(aborts.load(Ordering::SeqCst), 1);
    assert!(lock(&handle.connected).is_empty());
}

/// Mutation: check the launch deadline before relay transport preflight but not immediately
/// before the decisive ACK. The injected real writer spawn advances the shared monotonic clock
/// through expiry after all transport allocation succeeds.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn launch_deadline_expiring_during_real_preflight_refuses_without_starting() {
    let handle = Arc::new(ClaimLifecycleHandle::default());
    let commits = Arc::new(AtomicU64::new(0));
    let aborts = Arc::new(AtomicU64::new(0));
    let handler = Arc::new(ClaimLifecycleHandler {
        claimant: Mutex::new(None),
        handle: Arc::clone(&handle),
        commits: Arc::clone(&commits),
        aborts: Arc::clone(&aborts),
        fail_preparations: Arc::new(AtomicU64::new(0)),
    });
    let clock = Arc::new(ManualClock::default());
    let context = request_context();
    let service = Arc::new(
        NativeBootstrapService::with_clock_without_terminal_verification_for_tests(
            context.canonical_project().to_path_buf(),
            context.native_wire_version(),
            handler as Arc<dyn NativeBootstrapHandler>,
            Arc::clone(&clock) as Arc<dyn MonotonicClock>,
        ),
    );
    service.set_claimed_conn_writer_spawner_for_tests(move |name, task| {
        clock.set(LAUNCH_RESULT_TIMEOUT);
        std::thread::Builder::new().name(name).spawn(task)
    });

    assert_eq!(
        claim_lifecycle_roundtrip(&service, &context, ConnId(4_218)),
        WIRE_REFUSED,
        "deadline expiry during real preflight escaped behind WIRE_OK"
    );
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert_eq!(aborts.load(Ordering::SeqCst), 1);
    assert!(lock(&handle.connected).is_empty());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn same_authenticated_socket_claims_and_owns_the_exact_conn_lifecycle() {
    let handle = Arc::new(ClaimLifecycleHandle::default());
    let commits = Arc::new(AtomicU64::new(0));
    let handler = Arc::new(ClaimLifecycleHandler {
        claimant: Mutex::new(None),
        handle: Arc::clone(&handle),
        commits: Arc::clone(&commits),
        aborts: Arc::new(AtomicU64::new(0)),
        fail_preparations: Arc::new(AtomicU64::new(0)),
    });
    let context = request_context();
    let service = Arc::new(
        NativeBootstrapService::new_without_terminal_verification_for_task2_tests(
            context.canonical_project().to_path_buf(),
            context.native_wire_version(),
            Arc::clone(&handler) as Arc<dyn NativeBootstrapHandler>,
        ),
    );
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let conn = ConnId(4_201);
    let worker = {
        let service = Arc::clone(&service);
        let active = service.try_acquire_connection().unwrap();
        std::thread::spawn(move || service.serve_connection(conn, server, active))
    };
    let input = std::fs::File::open("/dev/null").unwrap();
    let output = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .unwrap();
    let capability =
        request_direct_cli_capability(&mut client, input.as_fd(), output.as_fd(), &context)
            .unwrap();
    let receipt = present_direct_cli_capability(&mut client, capability, &context).unwrap();
    write_native_claim_request(&mut client, &receipt).unwrap();
    let mut status = [WIRE_REFUSED];
    client.read_exact(&mut status).unwrap();
    assert_eq!(status, [WIRE_OK]);
    drop(client);
    worker.join().unwrap();

    let claimant = lock(&handler.claimant).expect("claim reached the handler");
    assert_eq!(claimant.conn, conn);
    assert_eq!(claimant.principal.uid, own_uid());
    assert_eq!(claimant.principal.pid, std::process::id());
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert_eq!(&*lock(&handle.connected), &[conn]);
    assert_eq!(&*lock(&handle.gone), &[conn]);
}

#[test]
fn pending_native_launch_is_exact_single_use_and_expires_from_publish() {
    let clock = Arc::new(ManualClock::default());
    let launches = Arc::new(PendingNativeLaunches::with_sources(
        Arc::new(SequenceRng::default()),
        Arc::clone(&clock) as Arc<dyn MonotonicClock>,
        Duration::from_secs(60),
    ));
    let peer = PeerIdentity {
        uid: 501,
        pid: 7001,
    };
    let binding = launch_binding(peer);
    let pending = launches.reserve(binding.clone()).unwrap();
    let ticket_bytes = pending.receipt().ticket().wire_bytes();
    let presented = NativeLaunchTicket::from_reserved(ticket_bytes).unwrap();
    assert!(launches.has_pending(binding.agent_id()));
    assert_eq!(
        launches.consume(
            &presented,
            binding.agent_id(),
            9,
            claimant(ConnId(77), peer)
        ),
        Err(NativeLaunchClaimError::Unpublished)
    );

    clock.set(Duration::from_secs(45));
    launches.publish(pending.receipt(), 9).unwrap();
    pending.commit();
    clock.set(Duration::from_secs(104));

    let wrong_agent = AgentId("019f0000-0000-7000-8000-000000000099".into());
    assert_eq!(
        launches.consume(&presented, &wrong_agent, 9, claimant(ConnId(77), peer)),
        Err(NativeLaunchClaimError::WrongBinding)
    );
    for wrong_peer in [
        PeerIdentity {
            uid: peer.uid + 1,
            pid: peer.pid,
        },
        PeerIdentity {
            uid: peer.uid,
            pid: peer.pid + 1,
        },
    ] {
        assert_eq!(
            launches.consume(
                &presented,
                binding.agent_id(),
                9,
                claimant(ConnId(77), wrong_peer),
            ),
            Err(NativeLaunchClaimError::WrongBinding)
        );
    }
    assert_eq!(
        launches.consume(
            &presented,
            binding.agent_id(),
            10,
            claimant(ConnId(77), peer)
        ),
        Err(NativeLaunchClaimError::WrongBinding)
    );
    let claim = launches
        .consume(
            &presented,
            binding.agent_id(),
            9,
            claimant(ConnId(77), peer),
        )
        .unwrap();
    assert_eq!(claim.agent_id(), binding.agent_id());
    assert_eq!(claim.conn(), ConnId(77));
    assert_eq!(claim.host_generation(), 9);
    assert_eq!(
        launches.consume(
            &presented,
            binding.agent_id(),
            9,
            claimant(ConnId(78), peer)
        ),
        Err(NativeLaunchClaimError::UnknownTicket)
    );

    let expiring = launches.reserve(binding.clone()).unwrap();
    let expired_ticket =
        NativeLaunchTicket::from_reserved(expiring.receipt().ticket().wire_bytes()).unwrap();
    launches.publish(expiring.receipt(), 11).unwrap();
    assert_eq!(
        launches.publish(expiring.receipt(), 12),
        Err(NativeLaunchReservationError::AlreadyPublished)
    );
    expiring.commit();
    clock.set(Duration::from_secs(164));
    assert_eq!(
        launches.consume(
            &expired_ticket,
            binding.agent_id(),
            11,
            claimant(ConnId(79), peer)
        ),
        Err(NativeLaunchClaimError::Expired)
    );
    assert!(!launches.has_pending(binding.agent_id()));

    let pruned = launches.reserve(binding.clone()).unwrap();
    launches.publish(pruned.receipt(), 12).unwrap();
    pruned.commit();
    clock.set(Duration::from_secs(225));
    assert!(!launches.has_pending(binding.agent_id()));
}

#[test]
fn panicking_request_cancel_still_revokes_real_pending_authority() {
    let clock = Arc::new(ManualClock::default());
    let launches = Arc::new(PendingNativeLaunches::with_sources(
        Arc::new(SequenceRng::default()),
        clock as Arc<dyn MonotonicClock>,
        Duration::from_secs(60),
    ));
    let binding = launch_binding(PeerIdentity {
        uid: 501,
        pid: 7_001,
    });
    let mut pending = launches.reserve(binding.clone()).unwrap();
    pending.on_cancel(|| panic!("injected request-local cancellation panic"));
    drop(pending);
    assert!(
        !launches.has_pending(binding.agent_id()),
        "request-local panic skipped the real ticket-authority revocation"
    );
}

#[test]
fn pending_native_launch_drop_revokes_and_ticket_generation_is_bounded() {
    let clock = Arc::new(ManualClock::default());
    let launches = Arc::new(PendingNativeLaunches::with_sources(
        Arc::new(SequenceRng::default()),
        Arc::clone(&clock) as Arc<dyn MonotonicClock>,
        Duration::from_secs(60),
    ));
    let binding = launch_binding(PeerIdentity { uid: 501, pid: 8 });
    let pending = launches.reserve(binding.clone()).unwrap();
    drop(pending);
    assert!(!launches.has_pending(binding.agent_id()));

    let zeros = Arc::new(PendingNativeLaunches::with_sources(
        Arc::new(RepeatingRng([0; 32])),
        clock as Arc<dyn MonotonicClock>,
        Duration::from_secs(60),
    ));
    assert_eq!(
        zeros.reserve(binding).unwrap_err(),
        NativeLaunchReservationError::TicketGenerationExhausted
    );
}

#[test]
fn pending_native_launch_retries_existing_ticket_collisions() {
    let peer = PeerIdentity { uid: 501, pid: 8 };
    let binding = launch_binding(peer);
    struct CollisionThenFresh(AtomicU64);
    impl CapabilityRng for CollisionThenFresh {
        fn fill(&self, bytes: &mut [u8]) -> Result<(), BootstrapError> {
            let call = self.0.fetch_add(1, Ordering::SeqCst);
            bytes.fill(if call < 2 { 7 } else { 8 });
            Ok(())
        }
    }
    let launches = Arc::new(PendingNativeLaunches::with_sources(
        Arc::new(CollisionThenFresh(AtomicU64::new(0))),
        Arc::new(ManualClock::default()),
        Duration::from_secs(60),
    ));
    let first = launches.reserve(binding.clone()).unwrap();
    let second = launches
        .reserve({
            let mut other = binding;
            other.agent_id = AgentId("019f0000-0000-7000-8000-000000000098".into());
            other
        })
        .unwrap();
    assert_ne!(
        first.receipt().ticket().wire_bytes(),
        second.receipt().ticket().wire_bytes()
    );
}

#[test]
fn pending_native_launch_concurrent_consume_has_one_winner() {
    let binding = launch_binding(PeerIdentity { uid: 501, pid: 8 });
    let launches = Arc::new(PendingNativeLaunches::with_sources(
        Arc::new(SequenceRng::default()),
        Arc::new(ManualClock::default()),
        Duration::from_secs(60),
    ));
    let pending = launches.reserve(binding.clone()).unwrap();
    let bytes = pending.receipt().ticket().wire_bytes();
    launches.publish(pending.receipt(), 9).unwrap();
    pending.commit();
    let barrier = Arc::new(Barrier::new(3));
    let workers: Vec<_> = [ConnId(71), ConnId(72)]
        .into_iter()
        .map(|conn| {
            let launches = Arc::clone(&launches);
            let binding = binding.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                launches.consume(
                    &NativeLaunchTicket::from_reserved(bytes).unwrap(),
                    binding.agent_id(),
                    9,
                    claimant(conn, binding.principal),
                )
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(NativeLaunchClaimError::UnknownTicket)))
            .count(),
        1
    );
}

#[test]
fn failed_claim_callback_preserves_the_ticket_for_retry() {
    let binding = launch_binding(PeerIdentity { uid: 501, pid: 8 });
    let launches = Arc::new(PendingNativeLaunches::with_sources(
        Arc::new(SequenceRng::default()),
        Arc::new(ManualClock::default()),
        Duration::from_secs(60),
    ));
    let pending = launches.reserve(binding.clone()).unwrap();
    let ticket =
        NativeLaunchTicket::from_reserved(pending.receipt().ticket().wire_bytes()).unwrap();
    launches.publish(pending.receipt(), 9).unwrap();
    pending.commit();
    assert_eq!(
        launches.claim_with(
            &ticket,
            binding.agent_id(),
            9,
            claimant(ConnId(1), binding.principal),
            || { Err::<(), _>(NativeLaunchClaimError::WriterBusy) },
        ),
        Err(NativeLaunchClaimError::WriterBusy)
    );
    launches
        .consume(
            &ticket,
            binding.agent_id(),
            9,
            claimant(ConnId(2), binding.principal),
        )
        .unwrap();
}

/// A project directory that **exists**, and is the same path in every process of this test binary.
///
/// The service refuses a request whose working directory is not a real directory of its project
/// before the handler sees it, so the fixture context has to name one; and the re-executed probes
/// compare a context they built against one the parent built, so the path cannot carry a pid. It is
/// created and never removed: a `Scratch` guard here would take the directory away from every
/// other test in the process the moment one of them returned. Non-UTF-8 bytes stay on the tail
/// and profile, where the platform's filesystem has no say.
fn request_project() -> PathBuf {
    let project = PathBuf::from(format!(
        "{}-{}",
        marion_testsupport::SCRATCH_ROOT,
        own_uid()
    ))
    .join("native-ctx");
    std::fs::create_dir_all(&project).expect("the fixture project directory exists");
    project
        .canonicalize()
        .expect("the fixture project directory canonicalises")
}

fn request_context() -> DirectNativeRequestContext {
    let project = request_project();
    DirectNativeRequestContext::new(
        project.clone(),
        project,
        OsString::from("atlas"),
        vec![OsString::from_vec(vec![b'a', 0xfe]), OsString::new()],
        OsString::from("xterm-256color"),
        7,
    )
    .with_environment([(
        OsString::from("PATH"),
        OsString::from_vec(b"/client-bin-\xfd".to_vec()),
    )])
}

fn hash() -> ContextHash {
    context_hash(&request_context())
}

fn refuse_before_effects(
    effects: &Effects,
    result: Result<AuthorizedNativeFacade<'_>, BootstrapError>,
    expected: impl FnOnce(&BootstrapError) -> bool,
) {
    match result {
        Ok(authorization) => effects.record_authorized(&authorization),
        Err(error) => assert!(expected(&error), "unexpected refusal: {error}"),
    }
    effects.assert_zero();
}

#[test]
fn peer_uid_and_pid_are_kernel_authenticated_and_uid_mismatch_precedes_reads() {
    let (_client, server) = UnixStream::pair().unwrap();
    let connection = NativeBootstrapConnection::authenticate(ConnId(1), server).unwrap();
    assert_eq!(connection.id(), ConnId(1));
    assert_eq!(connection.peer().uid(), own_uid());
    assert_eq!(connection.peer().pid(), std::process::id());

    let (_client, server) = UnixStream::pair().unwrap();
    server.set_nonblocking(true).unwrap();
    let fake = PeerIdentity {
        uid: own_uid().wrapping_add(1),
        pid: std::process::id(),
    };
    assert!(matches!(
        NativeBootstrapConnection::authenticate_identity(ConnId(2), server, fake),
        Err(BootstrapError::PeerUidMismatch)
    ));
}

#[test]
fn descriptor_roles_accept_read_write_stdio_but_refuse_insufficient_rights() {
    assert!(descriptor_roles_allow(OFlags::RDWR, OFlags::RDWR));
    assert!(descriptor_roles_allow(OFlags::RDONLY, OFlags::WRONLY));
    assert!(!descriptor_roles_allow(OFlags::WRONLY, OFlags::WRONLY));
    assert!(!descriptor_roles_allow(OFlags::RDONLY, OFlags::RDONLY));
}

#[test]
fn server_tty_verifier_refuses_nonterminal_rights_before_downstream_effects() {
    let input = rustix::fs::open(
        "/dev/null",
        OFlags::RDONLY | OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .unwrap();
    let output = rustix::fs::open(
        "/dev/null",
        OFlags::WRONLY | OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .unwrap();
    let peer = PeerIdentity {
        uid: own_uid(),
        pid: std::process::id(),
    };
    let effects = Effects::default();

    assert!(matches!(
        crate::native_tty::verify_bootstrap_tty(peer, [input, output]),
        Err(crate::native_tty::NativeTtyError::NotATerminal)
    ));
    effects.assert_zero();
}

#[test]
fn context_hash_has_a_fixed_vector_and_every_semantic_boundary_is_load_bearing() {
    let base = DirectNativeRequestContext::new(
        PathBuf::from(OsString::from_vec(b"/project-\xff".to_vec())),
        PathBuf::from(OsString::from_vec(b"/project-\xff/work-\xfd".to_vec())),
        OsString::from("atlas"),
        vec![OsString::from_vec(vec![b'a', 0xfe]), OsString::new()],
        OsString::from("xterm-256color"),
        7,
    );
    assert_eq!(
        context_hash(&base).0,
        [
            33, 190, 77, 123, 145, 157, 2, 180, 92, 210, 231, 2, 173, 235, 169, 172, 179, 211, 99,
            38, 149, 224, 29, 136, 202, 163, 70, 85, 169, 17, 253, 208,
        ]
    );
    assert_ne!(
        context_hash_for_domain(b"marion/direct-native-context/v2\0", &base),
        context_hash(&base)
    );

    let mut changed = base.clone();
    changed.canonical_project = PathBuf::from("/other");
    assert_ne!(context_hash(&changed), context_hash(&base));
    changed = base.clone();
    changed.selector = OsString::from("boreal");
    assert_ne!(context_hash(&changed), context_hash(&base));
    changed = base.clone();
    changed.opaque_tail = vec![OsString::from_vec(vec![b'a', 0xfe, 0])];
    assert_ne!(context_hash(&changed), context_hash(&base));
    changed = base.clone();
    changed.terminal_profile = OsString::from("vt100");
    assert_ne!(context_hash(&changed), context_hash(&base));
    changed = base.clone();
    changed.native_wire_version += 1;
    assert_ne!(context_hash(&changed), context_hash(&base));
    // The working directory is process state, not an environment value: a capability issued for
    // one directory is not consumable from another.
    changed = base.clone();
    changed.client_cwd = PathBuf::from(OsString::from_vec(b"/project-\xff/other".to_vec()));
    assert_ne!(context_hash(&changed), context_hash(&base));

    // Environment values are carried but are never hash inputs.
    let with_environment = base
        .clone()
        .with_environment([(OsString::from("PATH"), OsString::from("/elsewhere"))]);
    assert_ne!(with_environment, base);
    assert_eq!(context_hash(&with_environment), context_hash(&base));
}

#[test]
fn wire_round_trip_preserves_raw_context_and_rejects_a_forged_claimed_hash() {
    let context = request_context();
    let (mut client, server) = UnixStream::pair().unwrap();
    write_wire_request(
        &mut client,
        &WireRequest::Issue {
            context: context.clone(),
            claimed_hash: ContextHash([0x5a; 32]),
        },
    )
    .unwrap();
    let received = read_wire_request(&server).unwrap();
    assert!(matches!(
        received.authenticated_context(),
        Err(BootstrapError::NativeWireContextHashMismatch)
    ));

    let (mut client, server) = UnixStream::pair().unwrap();
    write_wire_request(
        &mut client,
        &WireRequest::Issue {
            context: context.clone(),
            claimed_hash: context_hash(&context),
        },
    )
    .unwrap();
    let (received, _, token) = read_wire_request(&server)
        .unwrap()
        .authenticated_context()
        .unwrap();
    assert_eq!(received, context);
    assert!(token.is_none());
}

fn encoded_context(context: &DirectNativeRequestContext) -> Vec<u8> {
    fn field(bytes: &mut Vec<u8>, value: &OsStr) {
        bytes.extend_from_slice(&(value.as_bytes().len() as u32).to_be_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }

    let mut bytes = Vec::new();
    field(&mut bytes, context.canonical_project.as_os_str());
    field(&mut bytes, &context.selector);
    bytes.extend_from_slice(&(context.opaque_tail.len() as u32).to_be_bytes());
    for argument in &context.opaque_tail {
        field(&mut bytes, argument);
    }
    field(&mut bytes, &context.terminal_profile);
    bytes.extend_from_slice(&context.native_wire_version.to_be_bytes());
    bytes.extend_from_slice(&(context.environment.len() as u32).to_be_bytes());
    for (name, value) in &context.environment {
        field(&mut bytes, name);
        field(&mut bytes, value);
    }
    field(&mut bytes, context.client_cwd.as_os_str());
    bytes
}

fn budget_context(argument_bytes: usize) -> DirectNativeRequestContext {
    DirectNativeRequestContext::new(
        PathBuf::from(OsString::from_vec(vec![b'p'; MAX_CONTEXT_FIELD_BYTES])),
        PathBuf::from("c"),
        OsString::from("s"),
        vec![OsString::from_vec(vec![b'a'; argument_bytes])],
        OsString::from_vec(vec![b't'; MAX_CONTEXT_FIELD_BYTES]),
        NATIVE_WIRE_VERSION,
    )
}

/// Fixed framing (argc, version, env count) plus the project and profile fields at their caps,
/// the one-byte selector, the one-byte working directory, and one length-prefixed argument.
const BUDGET_FRAMING_BYTES: usize =
    12 + (4 + MAX_CONTEXT_FIELD_BYTES) + (4 + 1) + 4 + (4 + MAX_CONTEXT_FIELD_BYTES) + (4 + 1);

#[test]
fn aggregate_context_budget_accepts_exact_boundary_and_rejects_cumulative_and_checked_overflow() {
    let exact = budget_context(MAX_CONTEXT_BYTES - BUDGET_FRAMING_BYTES);
    assert_eq!(
        checked_context_wire_size(&exact).unwrap(),
        MAX_CONTEXT_BYTES
    );
    let mut encoded = std::io::Cursor::new(encoded_context(&exact));
    assert_eq!(read_context(&mut encoded).unwrap(), exact);

    let over = budget_context(MAX_CONTEXT_BYTES - BUDGET_FRAMING_BYTES + 1);
    assert!(matches!(
        checked_context_wire_size(&over),
        Err(BootstrapError::NativeWireContextTooLarge)
    ));
    let mut encoded = std::io::Cursor::new(encoded_context(&over));
    assert!(matches!(
        read_context(&mut encoded),
        Err(BootstrapError::NativeWireContextTooLarge)
    ));

    let mut budget = ContextBudget::new();
    assert!(matches!(
        budget.consume(usize::MAX),
        Err(BootstrapError::NativeWireContextTooLarge)
    ));
}

/// Mutation: leave the environment outside the aggregate budget, or trust its element count. One
/// name/value pair costs its two length prefixes plus its bytes, and the last byte of the budget
/// is still the boundary when it is spent on the environment.
#[test]
fn environment_debits_the_same_aggregate_budget_and_is_bounded_by_count() {
    let pair_bytes = (4 + 1) + (4 + 1);
    let exact = budget_context(MAX_CONTEXT_BYTES - BUDGET_FRAMING_BYTES - pair_bytes)
        .with_environment([(OsString::from("E"), OsString::from("v"))]);
    assert_eq!(
        checked_context_wire_size(&exact).unwrap(),
        MAX_CONTEXT_BYTES
    );
    let mut encoded = std::io::Cursor::new(encoded_context(&exact));
    assert_eq!(read_context(&mut encoded).unwrap(), exact);

    let over = budget_context(MAX_CONTEXT_BYTES - BUDGET_FRAMING_BYTES - pair_bytes)
        .with_environment([(OsString::from("E"), OsString::from("vv"))]);
    assert!(matches!(
        checked_context_wire_size(&over),
        Err(BootstrapError::NativeWireContextTooLarge)
    ));
    let mut encoded = std::io::Cursor::new(encoded_context(&over));
    assert!(matches!(
        read_context(&mut encoded),
        Err(BootstrapError::NativeWireContextTooLarge)
    ));

    let too_many = request_context().with_environment(
        (0..=MAX_CONTEXT_ENVIRONMENT_ENTRIES)
            .map(|index| (OsString::from(format!("E{index}")), OsString::new())),
    );
    assert!(matches!(
        checked_context_wire_size(&too_many),
        Err(BootstrapError::NativeWireProtocol)
    ));
    let mut encoded = std::io::Cursor::new(encoded_context(&too_many));
    assert!(matches!(
        read_context(&mut encoded),
        Err(BootstrapError::NativeWireProtocol)
    ));
}

/// Mutation: leave the working directory outside the aggregate budget. It trails the environment
/// on the wire and costs its length prefix plus its bytes like every other field.
#[test]
fn the_client_cwd_debits_the_same_aggregate_budget() {
    let exact = budget_context(MAX_CONTEXT_BYTES - BUDGET_FRAMING_BYTES);
    let mut over = exact.clone();
    over.client_cwd = PathBuf::from("cc");
    assert!(matches!(
        checked_context_wire_size(&over),
        Err(BootstrapError::NativeWireContextTooLarge)
    ));
    let mut encoded = std::io::Cursor::new(encoded_context(&over));
    assert!(matches!(
        read_context(&mut encoded),
        Err(BootstrapError::NativeWireContextTooLarge)
    ));
    let mut encoded = std::io::Cursor::new(encoded_context(&exact));
    assert_eq!(read_context(&mut encoded).unwrap(), exact);
}

/// Mutation: strip `MARION_*` only on the client. A frame that still carries reserved identity is
/// refused by the decoder itself, before any hash comparison or handler could see it.
#[test]
fn reserved_marion_environment_on_the_wire_is_a_protocol_violation() {
    let mut forged =
        request_context().with_environment([(OsString::from("PATH"), OsString::from("/bin"))]);
    forged.environment.push((
        OsString::from("MARION_NODE_TOKEN"),
        OsString::from("forged"),
    ));
    let mut encoded = std::io::Cursor::new(encoded_context(&forged));
    assert!(matches!(
        read_context(&mut encoded),
        Err(BootstrapError::NativeWireProtocol)
    ));

    let (mut client, _server) = UnixStream::pair().unwrap();
    assert!(matches!(
        write_context(&mut client, &forged),
        Err(BootstrapError::NativeWireProtocol)
    ));
}

struct CountingClock {
    calls: AtomicU64,
    now: Duration,
}

impl MonotonicClock for CountingClock {
    fn now(&self) -> Duration {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.now
    }
}

#[test]
fn issue_samples_the_injected_clock_once_while_the_authority_is_locked() {
    let clock = Arc::new(CountingClock {
        calls: AtomicU64::new(0),
        now: Duration::from_secs(3),
    });
    let authority = CapabilityAuthority::with_sources(
        Arc::new(SequenceRng::default()),
        Arc::clone(&clock) as Arc<dyn MonotonicClock>,
    );
    let fixture = fixture(9);
    authority
        .issue(&fixture.connection, &fixture.terminal, "atlas", hash())
        .unwrap();
    assert_eq!(clock.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn native_handshakes_install_deadlines_and_the_active_cap_releases_exactly() {
    let counter = Arc::new(AtomicUsize::new(0));
    let permits: Vec<_> = (0..MAX_ACTIVE_NATIVE_CONNECTIONS)
        .map(|_| ActiveNativeConnection::acquire(&counter).unwrap())
        .collect();
    assert!(matches!(
        ActiveNativeConnection::acquire(&counter),
        Err(BootstrapError::NativeConnectionLimit)
    ));
    drop(permits);
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    drop(ActiveNativeConnection::acquire(&counter).unwrap());

    let effects = Arc::new(Effects::default());
    let handler = Arc::new(CountingHandler {
        terminal_verifications: AtomicU64::new(0),
        effects,
    });
    let timeout = Duration::from_millis(25);
    let service = NativeBootstrapService::with_handshake_timeout(
        request_context().canonical_project().to_path_buf(),
        request_context().native_wire_version(),
        handler as Arc<dyn NativeBootstrapHandler>,
        Arc::new(ManualClock::default()),
        timeout,
    );
    let (_client, server) = UnixStream::pair().unwrap();
    let probe = server.try_clone().unwrap();
    let active = service.try_acquire_connection().unwrap();
    service.serve_connection(ConnId(8), server, active);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    assert_eq!(probe.read_timeout().unwrap(), Some(timeout));
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    assert_eq!(probe.read_timeout().unwrap(), None);
    assert_eq!(probe.write_timeout().unwrap(), Some(timeout));
}

struct TrickleIo {
    clock: Arc<ManualClock>,
    read_calls: usize,
    write_calls: usize,
    read_timeouts: Vec<Duration>,
    write_timeouts: Vec<Duration>,
}

impl DeadlineTransport for TrickleIo {
    fn set_deadline_read_timeout(&mut self, timeout: Duration) -> std::io::Result<()> {
        self.read_timeouts.push(timeout);
        Ok(())
    }

    fn set_deadline_write_timeout(&mut self, timeout: Duration) -> std::io::Result<()> {
        self.write_timeouts.push(timeout);
        Ok(())
    }
}

impl Read for TrickleIo {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        self.read_calls += 1;
        output[0] = b'x';
        self.clock.set(Duration::from_secs(if self.read_calls == 1 {
            4
        } else {
            10
        }));
        Ok(1)
    }
}

impl Write for TrickleIo {
    fn write(&mut self, _input: &[u8]) -> std::io::Result<usize> {
        self.write_calls += 1;
        self.clock
            .set(Duration::from_secs(if self.write_calls == 1 {
                4
            } else {
                10
            }));
        Ok(1)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn trickle(clock: Arc<ManualClock>) -> TrickleIo {
    TrickleIo {
        clock,
        read_calls: 0,
        write_calls: 0,
        read_timeouts: Vec::new(),
        write_timeouts: Vec::new(),
    }
}

#[test]
fn handshake_uses_one_absolute_deadline_for_trickle_reads_writes_and_effects() {
    let clock = Arc::new(ManualClock::default());
    let deadline = HandshakeDeadline::new(
        Arc::clone(&clock) as Arc<dyn MonotonicClock>,
        Duration::from_secs(10),
    )
    .unwrap();
    let mut transport = DeadlineIo::new(trickle(Arc::clone(&clock)), &deadline);
    let mut bytes = [0; 3];
    let error = transport.read_exact(&mut bytes).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(transport.inner.read_calls, 2);
    assert_eq!(
        transport.inner.read_timeouts,
        [Duration::from_secs(10), Duration::from_secs(6)]
    );

    clock.set(Duration::ZERO);
    let deadline = HandshakeDeadline::new(
        Arc::clone(&clock) as Arc<dyn MonotonicClock>,
        Duration::from_secs(10),
    )
    .unwrap();
    let mut transport = DeadlineIo::new(trickle(Arc::clone(&clock)), &deadline);
    let error = transport.write_all(b"abc").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(transport.inner.write_calls, 2);
    assert_eq!(
        transport.inner.write_timeouts,
        [Duration::from_secs(10), Duration::from_secs(6)]
    );

    let downstream = AtomicU64::new(0);
    if deadline.require_remaining().is_ok() {
        downstream.fetch_add(1, Ordering::SeqCst);
    }
    assert_eq!(downstream.load(Ordering::SeqCst), 0);
}

struct MarkerTail {
    clock: Arc<ManualClock>,
    reads: usize,
}

impl DeadlineTransport for MarkerTail {
    fn set_deadline_read_timeout(&mut self, _timeout: Duration) -> std::io::Result<()> {
        Ok(())
    }

    fn set_deadline_write_timeout(&mut self, _timeout: Duration) -> std::io::Result<()> {
        Ok(())
    }
}

impl Read for MarkerTail {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let byte = [b'N', b'B'][self.reads];
        output[0] = byte;
        self.reads += 1;
        self.clock
            .set(Duration::from_secs(if self.reads == 1 { 4 } else { 10 }));
        Ok(1)
    }
}

impl Write for MarkerTail {
    fn write(&mut self, _input: &[u8]) -> std::io::Result<usize> {
        unreachable!("the marker tail is read-only")
    }

    fn flush(&mut self) -> std::io::Result<()> {
        unreachable!("the marker tail is read-only")
    }
}

#[test]
fn service_partial_descriptor_marker_expires_before_handler_effects() {
    let clock = Arc::new(ManualClock::default());
    let effects = Arc::new(Effects::default());
    let handler = Arc::new(CountingHandler {
        terminal_verifications: AtomicU64::new(0),
        effects: Arc::clone(&effects),
    });
    let service = NativeBootstrapService::with_handshake_timeout(
        request_context().canonical_project().to_path_buf(),
        request_context().native_wire_version(),
        Arc::clone(&handler) as Arc<dyn NativeBootstrapHandler>,
        Arc::clone(&clock) as Arc<dyn MonotonicClock>,
        Duration::from_secs(10),
    );
    let deadline = HandshakeDeadline::new(
        Arc::clone(&clock) as Arc<dyn MonotonicClock>,
        Duration::from_secs(10),
    )
    .unwrap();
    let (_client, server) = UnixStream::pair().unwrap();
    let error = service
        .serve_authenticated_with_receiver(ConnId(81), &server, &deadline, |_stream, deadline| {
            let mut message = [b'M', 0, 0, 0];
            let mut tail = DeadlineIo::new(
                MarkerTail {
                    clock: Arc::clone(&clock),
                    reads: 0,
                },
                deadline,
            );
            read_descriptor_message_tail(&mut tail, &mut message, 1)?;
            Err(BootstrapError::DescriptorMessage)
        })
        .unwrap_err();
    assert!(
        matches!(error, BootstrapError::NativeTransportIo(ref io) if io.kind() == std::io::ErrorKind::TimedOut)
    );
    assert_eq!(handler.terminal_verifications.load(Ordering::SeqCst), 0);
    assert_eq!(service.capability_lookup_count(), 0);
    effects.assert_zero();
}

#[test]
fn connection_limit_refuses_before_a_worker_can_be_spawned() {
    let service = NativeBootstrapService::disabled(PathBuf::from("/p"));
    let mut permits = Vec::new();
    while let Ok(permit) = service.try_acquire_connection() {
        permits.push(permit);
    }
    assert_eq!(permits.len(), MAX_ACTIVE_NATIVE_CONNECTIONS);

    let (mut client, mut server) = UnixStream::pair().unwrap();
    let spawned = AtomicU64::new(0);
    if service.admit_connection(&mut server).is_some() {
        spawned.fetch_add(1, Ordering::SeqCst);
    }
    let mut status = [WIRE_OK];
    client.read_exact(&mut status).unwrap();
    assert_eq!(status, [WIRE_REFUSED]);
    assert_eq!(spawned.load(Ordering::SeqCst), 0);
}

struct RepeatingRng([u8; 32]);

impl CapabilityRng for RepeatingRng {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), BootstrapError> {
        bytes.copy_from_slice(&self.0);
        Ok(())
    }
}

#[test]
fn capability_generation_has_a_bounded_collision_retry_limit() {
    let clock = Arc::new(ManualClock::default());
    let authority = CapabilityAuthority::with_sources(
        Arc::new(RepeatingRng([0x42; 32])),
        clock as Arc<dyn MonotonicClock>,
    );
    let fixture = fixture(12);
    authority
        .issue(&fixture.connection, &fixture.terminal, "atlas", hash())
        .unwrap();
    assert!(matches!(
        authority.issue(&fixture.connection, &fixture.terminal, "atlas", hash()),
        Err(BootstrapError::CapabilityEntropyExhausted)
    ));
}

#[test]
fn consumed_service_request_moves_once_into_native_selection_with_authenticated_state() {
    use marion_core::{
        Lane, NativeAdapterId, NativeFacadeDescriptor, NativeFacadeRegistry, NativeLane,
        VendorIdentity,
    };

    const DESCRIPTOR: NativeFacadeDescriptor = NativeFacadeDescriptor {
        identity: VendorIdentity::new("atlas"),
        command: "atlas",
        aliases: &[],
        native: Some(Lane::new(
            true,
            NativeLane::new("atlas", "codex", NativeAdapterId::new("atlas-native")),
        )),
        structured: None,
    };

    let clock = Arc::new(ManualClock::default());
    let authority = authority(clock);
    let fixture = fixture(13);
    let context = request_context();
    let hash = context_hash(&context);
    let token = authority
        .issue(&fixture.connection, &fixture.terminal, "atlas", hash)
        .unwrap()
        .wire_bytes();
    let authorization = authority
        .consume(&fixture.connection, &fixture.terminal, "atlas", hash, token)
        .unwrap();
    let Fixture {
        terminal,
        connection: _,
        _client: _,
    } = fixture;
    let request = ConsumedNativeRequest {
        context: &context,
        hash,
        terminal,
        authorization,
    };

    let parts = request.into_parts();
    assert_eq!(parts.context(), &context);
    assert_eq!(parts.hash(), hash);
    assert_eq!(parts.terminal().geometry, geometry());
    let (moved_context, moved_hash, terminal, authorization) = parts.into_components();
    assert_eq!(moved_context, &context);
    assert_eq!(moved_hash, hash);
    assert_eq!(terminal.geometry, geometry());
    let descriptors = [DESCRIPTOR];
    let registry = NativeFacadeRegistry::new(&descriptors).unwrap();
    let selected = crate::native_intent::select_consumed_native(&registry, authorization).unwrap();
    assert_eq!(selected.descriptor().command, "atlas");
}

#[test]
fn consumed_alias_hash_precedes_canonical_selector_resolution() {
    use marion_core::{
        Lane, NativeAdapterId, NativeFacadeDescriptor, NativeFacadeRegistry, NativeLane,
        VendorIdentity,
    };

    const DESCRIPTOR: NativeFacadeDescriptor = NativeFacadeDescriptor {
        identity: VendorIdentity::new("atlas"),
        command: "atlas",
        aliases: &["at"],
        native: Some(Lane::new(
            true,
            NativeLane::new("atlas", "codex", NativeAdapterId::new("atlas-native")),
        )),
        structured: None,
    };

    let clock = Arc::new(ManualClock::default());
    let authority = authority(clock);
    let fixture = fixture(131);
    let mut alias_context = request_context();
    alias_context.selector = OsString::from("at");
    alias_context.opaque_tail = vec![OsString::from_vec(b"--opaque-\xff".to_vec())];
    let alias_hash = context_hash(&alias_context);
    let mut canonical_context = alias_context.clone();
    canonical_context.selector = OsString::from("atlas");
    assert_ne!(alias_hash, context_hash(&canonical_context));

    let token = authority
        .issue(&fixture.connection, &fixture.terminal, "at", alias_hash)
        .unwrap()
        .wire_bytes();
    let authorization = authority
        .consume(
            &fixture.connection,
            &fixture.terminal,
            "at",
            alias_hash,
            token,
        )
        .unwrap();
    let descriptors = [DESCRIPTOR];
    let registry = NativeFacadeRegistry::new(&descriptors).unwrap();
    let selected = crate::native_intent::select_consumed_native(&registry, authorization).unwrap();
    assert_eq!(selected.descriptor().command, "atlas");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
const ROUTE_DESCRIPTOR: NativeFacadeDescriptor = NativeFacadeDescriptor {
    identity: VendorIdentity::new("atlas"),
    command: "atlas",
    aliases: &["at"],
    native: Some(Lane::new(
        true,
        NativeLane::new("atlas", "codex", NativeAdapterId::new("atlas-native")),
    )),
    structured: None,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct SelectingRouteHandler {
    selected_command: Mutex<Option<String>>,
    raw_context: Mutex<Option<(OsString, Vec<OsString>, ContextHash)>>,
    terminal: Mutex<Option<crate::native_tty::ControllingTtyWitness>>,
    effects: Effects,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl SelectingRouteHandler {
    fn new() -> Self {
        Self {
            selected_command: Mutex::new(None),
            raw_context: Mutex::new(None),
            terminal: Mutex::new(None),
            effects: Effects::default(),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl NativeBootstrapHandler for SelectingRouteHandler {
    fn verify_terminal(
        &self,
        _peer: PeerIdentity,
        _stdin: BorrowedFd<'_>,
        _stdout: BorrowedFd<'_>,
    ) -> Result<TerminalGeometryObservation, BootstrapError> {
        Ok(TerminalGeometryObservation::new(geometry()))
    }

    fn authorized(
        &self,
        request: ConsumedNativeRequest<'_>,
        _deadline: &NativeLaunchDeadline,
    ) -> Result<PendingNativeLaunchReceipt, BootstrapError> {
        assert_eq!(request.hash(), context_hash(request.context()));
        let expected_project = request.context().canonical_project.clone();
        let expected_principal = request.terminal.peer;
        let expected_conn = request.terminal.connection;
        let expected_fingerprint = request.terminal.fingerprint;
        let expected_geometry = request.terminal.geometry;
        *lock(&self.raw_context) = Some((
            request.context().selector.clone(),
            request.context().opaque_tail.clone(),
            request.hash(),
        ));
        let registry = NativeFacadeRegistry::new(&[ROUTE_DESCRIPTOR]).expect("route registry");
        let selected = select_authenticated_native_launch(&registry, request)
            .ok_or(BootstrapError::AuthorizationRefused)?;
        let (selected, terminal, binding) = selected.into_parts(AgentId(NATIVE_TEST_AGENT.into()));
        assert_eq!(binding.agent_id(), &AgentId(NATIVE_TEST_AGENT.into()));
        assert_eq!(
            binding.context_hash,
            lock(&self.raw_context).as_ref().unwrap().2
        );
        assert_eq!(binding.project, expected_project);
        assert_eq!(binding.principal, expected_principal);
        assert_eq!(binding.bootstrap_conn, expected_conn);
        assert_eq!(binding.terminal, expected_fingerprint);
        assert_eq!(binding.initial_geometry, expected_geometry);
        assert_eq!(binding.descriptor.vendor, "atlas");
        assert_eq!(binding.descriptor.command, "atlas");
        assert_eq!(binding.descriptor.adapter, "atlas-native");
        *lock(&self.selected_command) = Some(selected.descriptor().command.to_owned());
        *lock(&self.terminal) = Some(terminal);
        Ok(pending_receipt(NATIVE_TEST_AGENT))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_tty_route_child(mut command: std::process::Command) -> (std::process::ExitStatus, String) {
    use std::io::Read;

    let mut child = command.spawn().expect("spawn real TTY route probe");
    drop(command);
    let pid = child.id() as i32;
    let mut stderr = child.stderr.take().expect("probe control pipe");
    let (output_tx, output_rx) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut output = String::new();
        stderr
            .read_to_string(&mut output)
            .expect("read route probe");
        let _ = output_tx.send(output);
    });
    let (status_tx, status_rx) = std::sync::mpsc::sync_channel(1);
    let waiter = std::thread::spawn(move || {
        let _ = status_tx.send(child.wait());
    });
    let status = match status_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(status) => status.expect("wait for route probe"),
        Err(error) => {
            unsafe extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            const SIGKILL: i32 = 9;
            // SAFETY: `pid` is the exact owned child and SIGKILL is used only as a deadlock ceiling.
            let _ = unsafe { kill(pid, SIGKILL) };
            let _ = status_rx.recv_timeout(Duration::from_secs(2));
            panic!("real TTY route probe exceeded watchdog: {error}");
        }
    };
    waiter.join().expect("join route waiter");
    let output = output_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("route control reader exceeded watchdog");
    reader.join().expect("join route reader");
    (status, output)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct RevalidationProbeWatchdogOutcome {
    status: std::process::ExitStatus,
    timed_out: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_revalidation_probe_with_watchdog(
    child: &mut std::process::Child,
    deadlock_ceiling: Duration,
) -> RevalidationProbeWatchdogOutcome {
    let deadline = std::time::Instant::now()
        .checked_add(deadlock_ceiling)
        .expect("revalidation watchdog deadline");
    loop {
        if let Some(status) = child.try_wait().expect("poll revalidation probe") {
            return RevalidationProbeWatchdogOutcome {
                status,
                timed_out: false,
            };
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            child.kill().expect("kill timed-out revalidation probe");
            let status = child.wait().expect("reap timed-out revalidation probe");
            return RevalidationProbeWatchdogOutcome {
                status,
                timed_out: true,
            };
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(1)));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn real_tty_bootstrap_selects_alias_only_after_atomic_consume() {
    use crate::pty::PtyMaster;

    #[cfg(target_os = "linux")]
    const TIOCSCTTY: std::ffi::c_ulong = 0x540e;
    #[cfg(target_os = "macos")]
    const TIOCSCTTY: std::ffi::c_ulong = 0x2000_7461;

    unsafe extern "C" {
        fn setsid() -> i32;
        fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
        fn write(fd: i32, bytes: *const std::ffi::c_void, len: usize) -> isize;
    }

    let master = PtyMaster::open(crate::pty::WinSize::new(117, 43)).expect("route PTY");
    let stdin = master.open_slave().expect("route stdin");
    let stdout = master.open_slave().expect("route stdout");
    let mut command = std::process::Command::new(std::env::current_exe().expect("unit test path"));
    command
        .args([
            "--exact",
            "native_bootstrap::tests::real_tty_bootstrap_alias_probe",
            "--nocapture",
        ])
        .env("MARION_REAL_TTY_ROUTE_PROBE", "1")
        .stdin(std::process::Stdio::from(stdin))
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::piped());
    // SAFETY: only async-signal-safe session/ioctl/write calls run between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if setsid() < 0 || ioctl(0, TIOCSCTTY, 0_i32) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let marker = b"SESSION_READY\n";
            let _ = write(2, marker.as_ptr().cast(), marker.len());
            Ok(())
        });
    }

    let (status, output) = run_tty_route_child(command);
    assert!(status.success(), "real route probe failed: {output}");
    assert!(
        output.contains("SESSION_READY"),
        "missing readiness barrier: {output}"
    );
    assert!(
        output.contains("TTY_IDENTITY_READY"),
        "missing terminal identity barrier: {output}"
    );
    assert!(
        output.contains("REAL_ROUTE_OK"),
        "missing route result: {output}"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn real_tty_bootstrap_alias_probe() {
    if std::env::var_os("MARION_REAL_TTY_ROUTE_PROBE").is_none() {
        return;
    }

    let stdin = std::io::stdin();
    assert_eq!(
        rustix::termios::tcgetsid(stdin.as_fd()).unwrap(),
        getsid(None).unwrap()
    );
    assert_eq!(
        rustix::termios::tcgetpgrp(stdin.as_fd()).unwrap(),
        getpgrp()
    );
    eprintln!("TTY_IDENTITY_READY");

    let handler = Arc::new(SelectingRouteHandler::new());
    let work = marion_testsupport::scratch("native-alias-probe");
    let project = work.to_path_buf();
    let (client_stream, server_stream) = UnixStream::pair().expect("bootstrap socket pair");
    let service = Arc::new(NativeBootstrapService::new(
        project.clone(),
        NATIVE_WIRE_VERSION,
        Arc::clone(&handler) as Arc<dyn NativeBootstrapHandler>,
    ));
    let (stage_tx, stage_rx) = std::sync::mpsc::channel();
    let worker = {
        let service = Arc::clone(&service);
        let active = service.try_acquire_connection().expect("connection permit");
        std::thread::spawn(move || {
            let _hook = install_native_route_test_hook(stage_tx);
            service.serve_connection(ConnId(3_001), server_stream, active);
        })
    };
    let client = NativeBootstrapClient {
        stream: client_stream,
        canonical_project: project.clone(),
        client_cwd: project.clone(),
    };
    let registry = NativeFacadeRegistry::new(&[ROUTE_DESCRIPTOR]).expect("client registry");
    let opaque = OsString::from_vec(b"--opaque-\xff".to_vec());
    let mut stderr = Vec::new();
    let status = crate::facade_cli::dispatch_native_facade_or_legacy(
        [OsString::from("at"), opaque.clone()],
        &registry,
        move || Ok(client),
        |handoff| {
            let (receipt, _tty) = handoff.into_parts();
            assert_eq!(receipt.agent_id(), &AgentId(NATIVE_TEST_AGENT.into()));
            std::process::ExitCode::SUCCESS
        },
        &mut stderr,
        || panic!("registered selector reached legacy"),
    );
    worker.join().expect("join real bootstrap service");

    assert_eq!(
        status,
        std::process::ExitCode::SUCCESS,
        "stderr: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(
        stderr.is_empty(),
        "successful native route emitted a refusal"
    );
    assert_eq!(
        stage_rx.try_iter().collect::<Vec<_>>(),
        [
            NativeRouteTestStage::DescriptorsVerified,
            NativeRouteTestStage::CapabilityConsumed,
            NativeRouteTestStage::SelectorSelected,
        ]
    );
    assert_eq!(lock(&handler.selected_command).as_deref(), Some("atlas"));
    let (selector, opaque_tail, alias_hash) = lock(&handler.raw_context)
        .clone()
        .expect("authenticated raw context retained");
    assert_eq!(selector, OsString::from("at"));
    assert_eq!(opaque_tail, [opaque]);
    let canonical = DirectNativeRequestContext::new(
        project.clone(),
        project,
        OsString::from("atlas"),
        opaque_tail,
        std::env::var_os("TERM").unwrap_or_default(),
        NATIVE_WIRE_VERSION,
    );
    assert_ne!(alias_hash, context_hash(&canonical));
    assert!(
        lock(&handler.terminal).as_ref().is_some_and(|terminal| {
            let geometry = terminal.initial_geometry();
            geometry.cols == 117 && geometry.rows == 43
        }),
        "selected route did not retain the server controlling-terminal witness"
    );
    handler.effects.assert_zero();
    eprintln!("REAL_ROUTE_OK");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn control_line(reader: &mut std::io::BufReader<UnixStream>) -> String {
    use std::io::BufRead;

    let mut line = String::new();
    reader.read_line(&mut line).expect("read control barrier");
    assert!(!line.is_empty(), "control channel closed before a barrier");
    line.trim_end().to_owned()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn send_control(stream: &mut UnixStream, marker: &str) {
    stream
        .write_all(marker.as_bytes())
        .expect("write control marker");
    stream.write_all(b"\n").expect("terminate control marker");
    stream.flush().expect("flush control marker");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn consume_revalidates_the_real_peer_after_issue_before_any_route_effect() {
    use crate::pty::PtyMaster;

    #[cfg(target_os = "linux")]
    const TIOCSCTTY: std::ffi::c_ulong = 0x540e;
    #[cfg(target_os = "macos")]
    const TIOCSCTTY: std::ffi::c_ulong = 0x2000_7461;

    unsafe extern "C" {
        fn setsid() -> i32;
        fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
        fn write(fd: i32, bytes: *const std::ffi::c_void, len: usize) -> isize;
    }

    let master = PtyMaster::open(crate::pty::WinSize::new(117, 43)).expect("TOCTOU PTY");
    let stdin = master.open_slave().expect("session stdin");
    let stdout = master.open_slave().expect("session stdout");
    let mut command = std::process::Command::new(std::env::current_exe().expect("unit test path"));
    command
        .args([
            "--exact",
            "native_bootstrap::tests::real_tty_revalidation_session_helper",
            "--nocapture",
        ])
        .env("MARION_REAL_TTY_REVALIDATION_SESSION", "1")
        .stdin(std::process::Stdio::from(stdin))
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::piped());
    // SAFETY: only async-signal-safe session/ioctl/write calls run between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if setsid() < 0 || ioctl(0, TIOCSCTTY, 0_i32) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let marker = b"REVALIDATION_SESSION_READY\n";
            let _ = write(2, marker.as_ptr().cast(), marker.len());
            Ok(())
        });
    }

    let (status, output) = run_tty_route_child(command);
    assert!(
        status.success(),
        "real revalidation fixture failed: {output}"
    );
    for marker in [
        "REVALIDATION_SESSION_READY",
        "REVALIDATION_SESSION_IDENTIFIED",
        "REVALIDATION_PROBE_FOREGROUND",
        "CAPABILITY_ISSUED",
        "PEER_BACKGROUNDED",
        "CONSUME_REFUSED",
    ] {
        assert!(output.contains(marker), "missing {marker}: {output}");
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn revalidation_probe_watchdog_reaps_a_probe_parked_after_refusal() {
    use std::io::{BufRead, Read};
    use std::process::Stdio;

    let mut command = std::process::Command::new(std::env::current_exe().expect("unit test path"));
    command
        .args([
            "--exact",
            "native_bootstrap::tests::real_tty_revalidation_watchdog_probe",
            "--nocapture",
        ])
        .env("MARION_REAL_TTY_REVALIDATION_WATCHDOG_PROBE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn parked revalidation probe");
    drop(command);
    let stderr = child.stderr.take().expect("probe marker pipe");
    let (marker_tx, marker_rx) = std::sync::mpsc::sync_channel(1);
    let (eof_tx, eof_rx) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stderr);
        let mut marker = String::new();
        reader.read_line(&mut marker).expect("read refusal marker");
        let _ = marker_tx.send(marker);
        let mut remainder = String::new();
        let _ = eof_tx.send(reader.read_to_string(&mut remainder));
    });
    let marker = match marker_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(marker) => marker,
        Err(error) => {
            let _ = wait_for_revalidation_probe_with_watchdog(&mut child, Duration::ZERO);
            eof_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("refusal marker reader did not reach EOF")
                .expect("drain refusal marker pipe");
            reader.join().expect("join refusal marker reader");
            panic!("probe did not reach REFUSED: {error}");
        }
    };

    let outcome = wait_for_revalidation_probe_with_watchdog(&mut child, Duration::from_millis(25));
    eof_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("parked probe reader did not reach EOF after child reap")
        .expect("drain refusal marker pipe");
    reader.join().expect("join bounded refusal marker reader");
    assert_eq!(marker.trim_end(), "REFUSED");
    assert!(outcome.timed_out, "parked probe escaped its watchdog");
    assert!(
        !outcome.status.success(),
        "watchdog did not kill its exact owned probe"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn real_tty_revalidation_watchdog_probe() {
    if std::env::var_os("MARION_REAL_TTY_REVALIDATION_WATCHDOG_PROBE").is_none() {
        return;
    }
    eprintln!("REFUSED");
    loop {
        std::thread::park();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn real_tty_revalidation_session_helper() {
    use std::os::fd::{AsFd, AsRawFd};
    use std::process::Stdio;

    if std::env::var_os("MARION_REAL_TTY_REVALIDATION_SESSION").is_none() {
        return;
    }

    assert_eq!(
        rustix::termios::tcgetsid(std::io::stdin().as_fd()).unwrap(),
        getsid(None).unwrap()
    );
    assert_eq!(
        rustix::termios::tcgetpgrp(std::io::stdin().as_fd()).unwrap(),
        getpgrp()
    );
    eprintln!("REVALIDATION_SESSION_IDENTIFIED");

    unsafe extern "C" {
        fn signal(signal: i32, handler: usize) -> usize;
    }
    const SIGTTOU: i32 = 22;
    const SIG_IGN: usize = 1;
    // SAFETY: this fixture process owns no prior SIGTTOU handler and exits after the scenario.
    let _ = unsafe { signal(SIGTTOU, SIG_IGN) };

    const CONTROL_FD: i32 = 9;
    let (mut control, probe_control) = UnixStream::pair().expect("revalidation control pair");
    let probe_control_fd = probe_control.as_raw_fd();
    let mut command = std::process::Command::new(std::env::current_exe().expect("unit test path"));
    command
        .args([
            "--exact",
            "native_bootstrap::tests::real_tty_revalidation_probe",
            "--nocapture",
        ])
        .env("MARION_REAL_TTY_REVALIDATION_PROBE", "1")
        .env(
            "MARION_REAL_TTY_REVALIDATION_CONTROL_FD",
            CONTROL_FD.to_string(),
        )
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0);
    // SAFETY: `dup2` is async-signal-safe and the captured descriptor remains owned until spawn.
    unsafe {
        command.pre_exec(move || {
            unsafe extern "C" {
                fn dup2(old: i32, new: i32) -> i32;
            }
            if dup2(probe_control_fd, CONTROL_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("spawn revalidation probe");
    drop(command);
    drop(probe_control);
    let probe_pgid = Pid::from_raw(child.id() as i32).expect("probe pid");
    control
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("bound control reads");
    let mut reader = std::io::BufReader::new(control.try_clone().expect("clone control reader"));
    assert_eq!(control_line(&mut reader), "PROBE_READY");
    rustix::termios::tcsetpgrp(std::io::stdin().as_fd(), probe_pgid)
        .expect("make probe foreground");
    send_control(&mut control, "FOREGROUND");
    eprintln!("REVALIDATION_PROBE_FOREGROUND");

    assert_eq!(control_line(&mut reader), "ISSUED");
    eprintln!("CAPABILITY_ISSUED");
    rustix::termios::tcsetpgrp(std::io::stdin().as_fd(), getpgrp())
        .expect("background probe before consume");
    send_control(&mut control, "BACKGROUND");
    eprintln!("PEER_BACKGROUNDED");
    assert_eq!(control_line(&mut reader), "REFUSED");
    eprintln!("CONSUME_REFUSED");
    let outcome = wait_for_revalidation_probe_with_watchdog(&mut child, Duration::from_secs(5));
    assert!(
        !outcome.timed_out,
        "revalidation probe exceeded its watchdog"
    );
    assert!(
        outcome.status.success(),
        "revalidation probe failed: {}",
        outcome.status
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn real_tty_revalidation_probe() {
    use std::os::fd::{FromRawFd, OwnedFd};

    if std::env::var_os("MARION_REAL_TTY_REVALIDATION_PROBE").is_none() {
        return;
    }

    let control_fd: i32 = std::env::var("MARION_REAL_TTY_REVALIDATION_CONTROL_FD")
        .expect("control descriptor")
        .parse()
        .expect("numeric control descriptor");
    // SAFETY: the session helper duplicated its owned stream endpoint to this exact descriptor
    // immediately before exec, and this probe takes its sole post-exec ownership.
    let control_fd = unsafe { OwnedFd::from_raw_fd(control_fd) };
    let mut control = UnixStream::from(control_fd);
    control
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("bound control reads");
    let mut reader = std::io::BufReader::new(control.try_clone().expect("clone control reader"));
    send_control(&mut control, "PROBE_READY");
    assert_eq!(control_line(&mut reader), "FOREGROUND");
    assert_eq!(
        rustix::termios::tcgetsid(std::io::stdin().as_fd()).unwrap(),
        getsid(None).unwrap()
    );
    assert_eq!(
        rustix::termios::tcgetpgrp(std::io::stdin().as_fd()).unwrap(),
        getpgrp()
    );

    let effects = Arc::new(Effects::default());
    let handler = Arc::new(CountingHandler {
        terminal_verifications: AtomicU64::new(0),
        effects: Arc::clone(&effects),
    });
    let work = marion_testsupport::scratch("native-revalidation-probe");
    let project = work.to_path_buf();
    let (client_stream, server_stream) = UnixStream::pair().expect("bootstrap socket pair");
    let service = Arc::new(NativeBootstrapService::new(
        project.clone(),
        NATIVE_WIRE_VERSION,
        Arc::clone(&handler) as Arc<dyn NativeBootstrapHandler>,
    ));
    let worker = {
        let service = Arc::clone(&service);
        let active = service.try_acquire_connection().expect("connection permit");
        std::thread::spawn(move || service.serve_connection(ConnId(3_002), server_stream, active))
    };
    let client = NativeBootstrapClient {
        stream: client_stream,
        canonical_project: project.clone(),
        client_cwd: project.clone(),
    };
    let context = DirectNativeRequestContext::new(
        project.clone(),
        project,
        OsString::from("atlas"),
        vec![OsString::from("--opaque")],
        std::env::var_os("TERM").unwrap_or_default(),
        NATIVE_WIRE_VERSION,
    );
    let witness = crate::native_tty::capture_process_stdio().expect("foreground client witness");
    let session = witness
        .bootstrap(client, context)
        .expect("capability issuance");
    send_control(&mut control, "ISSUED");
    assert_eq!(control_line(&mut reader), "BACKGROUND");
    assert!(matches!(
        session.consume(),
        Err(BootstrapError::AuthorizationRefused)
    ));
    worker.join().expect("join rejecting service");
    assert_eq!(handler.terminal_verifications.load(Ordering::SeqCst), 1);
    effects.assert_zero();
    send_control(&mut control, "REFUSED");
}

#[test]
fn service_policy_rejects_wrong_project_and_wire_version() {
    let service = NativeBootstrapService::disabled(PathBuf::from("/expected"));
    let wrong_project = DirectNativeRequestContext::new(
        PathBuf::from("/wrong"),
        PathBuf::from("/wrong"),
        OsString::from("atlas"),
        Vec::new(),
        OsString::from("xterm"),
        NATIVE_WIRE_VERSION,
    );
    assert!(matches!(
        service.validate_context(&wrong_project),
        Err(BootstrapError::NativeWireProjectMismatch)
    ));
    let wrong_version = DirectNativeRequestContext::new(
        PathBuf::from("/expected"),
        PathBuf::from("/expected"),
        OsString::from("atlas"),
        Vec::new(),
        OsString::from("xterm"),
        NATIVE_WIRE_VERSION + 1,
    );
    assert!(matches!(
        service.validate_context(&wrong_version),
        Err(BootstrapError::NativeWireVersionUnsupported)
    ));
}

/// Mutation: run the vendor wherever the client says, check only that the directory exists, or
/// accept every directory whose §2 key matches — which admits the common dir itself, the exact
/// `.git` the demo saw every harness print as its workspace.
#[test]
fn a_client_cwd_outside_the_project_is_refused() {
    let work = marion_testsupport::scratch("native-client-cwd");
    let project = work.join("project");
    let inside = project.join("src");
    let elsewhere = work.join("elsewhere");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    let context = |cwd: &Path, project: &Path| {
        DirectNativeRequestContext::new(
            project.to_path_buf(),
            cwd.to_path_buf(),
            OsString::from("atlas"),
            Vec::new(),
            OsString::from("xterm"),
            NATIVE_WIRE_VERSION,
        )
    };
    let refused = |service: &NativeBootstrapService, cwd: &Path, project: &Path| {
        assert!(
            matches!(
                service.validate_context(&context(cwd, project)),
                Err(BootstrapError::NativeWireClientCwdOutsideProject)
            ),
            "{} was accepted as a working directory of {}",
            cwd.display(),
            project.display()
        );
    };

    // Outside git the key is the directory itself, so that directory alone belongs to it.
    let service = NativeBootstrapService::disabled(project.clone());
    service
        .validate_context(&context(&project, &project))
        .expect("the project directory is its own working tree");
    for cwd in [&inside, &elsewhere, &work.join("missing")] {
        refused(&service, cwd, &project);
    }

    // In a repository the key is the common dir: every directory of the working tree resolves to
    // it, and so does the common dir itself, which is refused for lying inside the key.
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&project)
        .status()
        .expect("git runs");
    assert!(status.success(), "git init: {status}");
    let key = socket::project_root(&project);
    assert_eq!(key, project.join(".git"));
    let service = NativeBootstrapService::disabled(key.clone());
    for cwd in [&project, &inside] {
        service
            .validate_context(&context(cwd, &key))
            .unwrap_or_else(|error| panic!("{} was refused: {error}", cwd.display()));
    }
    for cwd in [&key, &key.join("objects"), &elsewhere] {
        refused(&service, cwd, &key);
    }
}

#[test]
fn capability_refusals_bind_every_field_and_leave_real_downstream_handler_untouched() {
    let clock = Arc::new(ManualClock::default());
    clock.set(Duration::from_secs(20));
    let authority = authority(clock);
    let first = fixture(10);
    let second = fixture(11);
    let effects = Effects::default();
    let hash = hash();

    let cases = [
        (
            "selector",
            authority
                .issue(&first.connection, &first.terminal, "atlas", hash)
                .unwrap()
                .wire_bytes(),
        ),
        (
            "hash",
            authority
                .issue(&first.connection, &first.terminal, "atlas", hash)
                .unwrap()
                .wire_bytes(),
        ),
        (
            "connection",
            authority
                .issue(&first.connection, &first.terminal, "atlas", hash)
                .unwrap()
                .wire_bytes(),
        ),
        (
            "peer",
            authority
                .issue(&first.connection, &first.terminal, "atlas", hash)
                .unwrap()
                .wire_bytes(),
        ),
        (
            "terminal",
            authority
                .issue(&first.connection, &first.terminal, "atlas", hash)
                .unwrap()
                .wire_bytes(),
        ),
        (
            "geometry",
            authority
                .issue(&first.connection, &first.terminal, "atlas", hash)
                .unwrap()
                .wire_bytes(),
        ),
    ];
    refuse_before_effects(
        &effects,
        authority.consume(
            &first.connection,
            &first.terminal,
            "boreal",
            hash,
            cases[0].1,
        ),
        |e| matches!(e, BootstrapError::CapabilityBinding),
    );
    refuse_before_effects(
        &effects,
        authority.consume(
            &first.connection,
            &first.terminal,
            "atlas",
            ContextHash([9; 32]),
            cases[1].1,
        ),
        |e| matches!(e, BootstrapError::CapabilityBinding),
    );
    refuse_before_effects(
        &effects,
        authority.consume(
            &second.connection,
            &second.terminal,
            "atlas",
            hash,
            cases[2].1,
        ),
        |e| matches!(e, BootstrapError::CapabilityBinding),
    );
    let wrong_peer = PeerIdentity {
        uid: first.connection.peer.uid,
        pid: first.connection.peer.pid.wrapping_add(1),
    };
    let wrong_peer_connection = NativeBootstrapConnection {
        id: first.connection.id,
        peer: wrong_peer,
        stream: first.connection.stream.try_clone().unwrap(),
    };
    let mut wrong_peer_terminal = fixture(10).terminal;
    wrong_peer_terminal.connection = wrong_peer_connection.id;
    wrong_peer_terminal.peer = wrong_peer;
    wrong_peer_terminal.fingerprint = first.terminal.fingerprint;
    refuse_before_effects(
        &effects,
        authority.consume(
            &wrong_peer_connection,
            &wrong_peer_terminal,
            "atlas",
            hash,
            cases[3].1,
        ),
        |e| matches!(e, BootstrapError::CapabilityBinding),
    );
    let mut wrong_terminal = fixture(10).terminal;
    wrong_terminal.peer = first.connection.peer;
    wrong_terminal.connection = first.connection.id;
    wrong_terminal.fingerprint.inode = wrong_terminal.fingerprint.inode.wrapping_add(1);
    refuse_before_effects(
        &effects,
        authority.consume(
            &first.connection,
            &wrong_terminal,
            "atlas",
            hash,
            cases[4].1,
        ),
        |e| matches!(e, BootstrapError::CapabilityBinding),
    );
    let mut wrong_geometry = fixture(10).terminal;
    wrong_geometry.peer = first.connection.peer;
    wrong_geometry.connection = first.connection.id;
    wrong_geometry.fingerprint = first.terminal.fingerprint;
    wrong_geometry.geometry.cols += 1;
    refuse_before_effects(
        &effects,
        authority.consume(
            &first.connection,
            &wrong_geometry,
            "atlas",
            hash,
            cases[5].1,
        ),
        |e| matches!(e, BootstrapError::CapabilityBinding),
    );
}

#[test]
fn expiry_replay_cleanup_and_concurrent_consumption_are_atomic() {
    let clock = Arc::new(ManualClock::default());
    clock.set(Duration::from_secs(5));
    let authority = Arc::new(authority(Arc::clone(&clock)));
    let fixture = Arc::new(fixture(20));
    let hash = hash();
    let before = authority
        .issue(&fixture.connection, &fixture.terminal, "atlas", hash)
        .unwrap()
        .wire_bytes();
    let boundary = authority
        .issue(&fixture.connection, &fixture.terminal, "atlas", hash)
        .unwrap()
        .wire_bytes();
    clock.set(Duration::from_nanos(14_999_999_999));
    authority
        .consume(
            &fixture.connection,
            &fixture.terminal,
            "atlas",
            hash,
            before,
        )
        .unwrap();
    clock.set(Duration::from_secs(15));
    assert!(matches!(
        authority.consume(
            &fixture.connection,
            &fixture.terminal,
            "atlas",
            hash,
            boundary
        ),
        Err(BootstrapError::CapabilityExpired)
    ));

    let stale = authority
        .issue(&fixture.connection, &fixture.terminal, "atlas", hash)
        .unwrap()
        .wire_bytes();
    clock.set(Duration::from_secs(25));
    let _fresh = authority
        .issue(&fixture.connection, &fixture.terminal, "atlas", hash)
        .unwrap();
    assert_eq!(lock(&authority.capabilities).len(), 1);
    assert!(matches!(
        authority.consume(&fixture.connection, &fixture.terminal, "atlas", hash, stale),
        Err(BootstrapError::CapabilityUnknownOrUsed)
    ));

    clock.set(Duration::from_secs(30));
    let replay = authority
        .issue(&fixture.connection, &fixture.terminal, "atlas", hash)
        .unwrap()
        .wire_bytes();
    authority
        .consume(
            &fixture.connection,
            &fixture.terminal,
            "atlas",
            hash,
            replay,
        )
        .unwrap();
    assert!(matches!(
        authority.consume(
            &fixture.connection,
            &fixture.terminal,
            "atlas",
            hash,
            replay
        ),
        Err(BootstrapError::CapabilityUnknownOrUsed)
    ));

    let concurrent = authority
        .issue(&fixture.connection, &fixture.terminal, "atlas", hash)
        .unwrap()
        .wire_bytes();
    let barrier = Arc::new(Barrier::new(8));
    let successes = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let authority = Arc::clone(&authority);
                let fixture = Arc::clone(&fixture);
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    authority
                        .consume(
                            &fixture.connection,
                            &fixture.terminal,
                            "atlas",
                            hash,
                            concurrent,
                        )
                        .is_ok()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|success| *success)
            .count()
    });
    assert_eq!(successes, 1);

    let _abandoned = authority
        .issue(&fixture.connection, &fixture.terminal, "atlas", hash)
        .unwrap();
    assert!(!lock(&authority.capabilities).is_empty());
    authority.revoke_connection(fixture.connection.id);
    assert!(lock(&authority.capabilities).is_empty());
}

struct CountingHandler {
    terminal_verifications: AtomicU64,
    effects: Arc<Effects>,
}

impl NativeBootstrapHandler for CountingHandler {
    fn verify_terminal(
        &self,
        _peer: PeerIdentity,
        _stdin: BorrowedFd<'_>,
        _stdout: BorrowedFd<'_>,
    ) -> Result<TerminalGeometryObservation, BootstrapError> {
        self.terminal_verifications.fetch_add(1, Ordering::SeqCst);
        Ok(TerminalGeometryObservation::new(geometry()))
    }

    fn authorized(
        &self,
        request: ConsumedNativeRequest<'_>,
        _deadline: &NativeLaunchDeadline,
    ) -> Result<PendingNativeLaunchReceipt, BootstrapError> {
        assert_eq!(request.hash(), context_hash(request.context()));
        self.effects.record_authorized(request.authorization());
        Ok(pending_receipt(NATIVE_TEST_AGENT))
    }
}

struct LateLaunchDecisionHandler {
    clock: Arc<ManualClock>,
    refuse: bool,
}

impl NativeBootstrapHandler for LateLaunchDecisionHandler {
    fn verify_terminal(
        &self,
        _peer: PeerIdentity,
        _stdin: BorrowedFd<'_>,
        _stdout: BorrowedFd<'_>,
    ) -> Result<TerminalGeometryObservation, BootstrapError> {
        self.clock.set(Duration::from_secs(9));
        Ok(TerminalGeometryObservation::new(geometry()))
    }

    fn authorized(
        &self,
        _request: ConsumedNativeRequest<'_>,
        deadline: &NativeLaunchDeadline,
    ) -> Result<PendingNativeLaunchReceipt, BootstrapError> {
        assert_eq!(deadline.require_remaining().unwrap(), LAUNCH_RESULT_TIMEOUT);
        self.clock.set(HANDSHAKE_TIMEOUT);
        if self.refuse {
            Err(BootstrapError::AuthorizationRefused)
        } else {
            Ok(pending_receipt(NATIVE_TEST_AGENT))
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn launch_success_and_handler_refusal_use_a_fresh_result_deadline() {
    for refuse in [false, true] {
        let clock = Arc::new(ManualClock::default());
        let handler = Arc::new(LateLaunchDecisionHandler {
            clock: Arc::clone(&clock),
            refuse,
        });
        let service = Arc::new(
            NativeBootstrapService::with_clock_without_terminal_verification_for_tests(
                request_context().canonical_project().to_path_buf(),
                request_context().native_wire_version(),
                handler,
                clock,
            ),
        );
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let worker = {
            let service = Arc::clone(&service);
            let active = service.try_acquire_connection().unwrap();
            std::thread::spawn(move || {
                service.serve_connection(ConnId(3_100 + u64::from(refuse)), server, active)
            })
        };
        let input = std::fs::File::open("/dev/null").unwrap();
        let output = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap();
        let context = request_context();
        let capability =
            request_direct_cli_capability(&mut client, input.as_fd(), output.as_fd(), &context)
                .unwrap();
        let result = present_direct_cli_capability(&mut client, capability, &context);
        if refuse {
            assert!(matches!(result, Err(BootstrapError::AuthorizationRefused)));
        } else {
            assert_eq!(
                result.unwrap().agent_id(),
                &AgentId(NATIVE_TEST_AGENT.into())
            );
        }
        worker.join().unwrap();
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn unsupported_platform_refuses_before_receiving_descriptors_or_minting_authority() {
    let (client, server) = UnixStream::pair().unwrap();
    let connection = NativeBootstrapConnection::authenticate(ConnId(30), server).unwrap();
    let input = std::fs::File::open("/dev/null").unwrap();
    let output = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .unwrap();
    send_terminal_descriptors(&client, input.as_fd(), output.as_fd()).unwrap();
    assert!(matches!(
        connection.receive_terminal(geometry()),
        Err(BootstrapError::AtomicCloexecUnsupported)
    ));
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn unsupported_service_closes_the_wire_with_zero_verification_authority_or_effects() {
    let effects = Arc::new(Effects::default());
    let handler = Arc::new(CountingHandler {
        terminal_verifications: AtomicU64::new(0),
        effects: Arc::clone(&effects),
    });
    let service = Arc::new(
        NativeBootstrapService::new_without_terminal_verification_for_task2_tests(
            request_context().canonical_project().to_path_buf(),
            request_context().native_wire_version(),
            Arc::clone(&handler) as Arc<dyn NativeBootstrapHandler>,
        ),
    );
    let (mut client, server) = UnixStream::pair().unwrap();
    let worker = {
        let service = Arc::clone(&service);
        let active = service.try_acquire_connection().unwrap();
        std::thread::spawn(move || service.serve_connection(ConnId(31), server, active))
    };
    let mut status = [WIRE_OK];
    client.read_exact(&mut status).unwrap();
    assert_eq!(status, [WIRE_REFUSED]);
    worker.join().unwrap();
    assert_eq!(handler.terminal_verifications.load(Ordering::SeqCst), 0);
    assert_eq!(service.capability_lookup_count(), 0);
    effects.assert_zero();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn native_wire_issues_and_consumes_only_after_the_verified_handoff() {
    let effects = Arc::new(Effects::default());
    let handler = Arc::new(CountingHandler {
        terminal_verifications: AtomicU64::new(0),
        effects: Arc::clone(&effects),
    });
    let service = Arc::new(
        NativeBootstrapService::new_without_terminal_verification_for_task2_tests(
            request_context().canonical_project().to_path_buf(),
            request_context().native_wire_version(),
            Arc::clone(&handler) as Arc<dyn NativeBootstrapHandler>,
        ),
    );
    let (mut client, server) = UnixStream::pair().unwrap();
    let worker = {
        let service = Arc::clone(&service);
        let active = service.try_acquire_connection().unwrap();
        std::thread::spawn(move || service.serve_connection(ConnId(32), server, active))
    };
    let input = std::fs::File::open("/dev/null").unwrap();
    let output = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .unwrap();
    let context = request_context();
    let capability =
        request_direct_cli_capability(&mut client, input.as_fd(), output.as_fd(), &context)
            .unwrap();
    let receipt = present_direct_cli_capability(&mut client, capability, &context).unwrap();
    assert_eq!(
        receipt.agent_id(),
        &marion_core::contract::AgentId(NATIVE_TEST_AGENT.into()),
        "bootstrap success must name the exact node whose writer priority it reserved"
    );
    worker.join().unwrap();
    assert_eq!(handler.terminal_verifications.load(Ordering::SeqCst), 1);
    assert_eq!(effects.descriptor_details.load(Ordering::SeqCst), 1);
    assert_eq!(effects.filesystem.load(Ordering::SeqCst), 1);
    assert_eq!(effects.processes.load(Ordering::SeqCst), 1);
    assert_eq!(effects.artifacts.load(Ordering::SeqCst), 1);
}

struct EnvironmentCapturingHandler {
    captured: Mutex<Option<Vec<(OsString, OsString)>>>,
}

impl NativeBootstrapHandler for EnvironmentCapturingHandler {
    fn verify_terminal(
        &self,
        _peer: PeerIdentity,
        _stdin: BorrowedFd<'_>,
        _stdout: BorrowedFd<'_>,
    ) -> Result<TerminalGeometryObservation, BootstrapError> {
        Ok(TerminalGeometryObservation::new(geometry()))
    }

    fn authorized(
        &self,
        request: ConsumedNativeRequest<'_>,
        _deadline: &NativeLaunchDeadline,
    ) -> Result<PendingNativeLaunchReceipt, BootstrapError> {
        assert_eq!(request.hash(), context_hash(request.context()));
        *lock(&self.captured) = Some(request.environment().to_vec());
        Ok(pending_receipt(NATIVE_TEST_AGENT))
    }
}

/// Mutation: read the environment off the supervisor process, or let `MARION_*` identity ride
/// the wire. The authorized handler must see exactly the client's stripped environment, byte for
/// byte, while the hash it authenticated stays independent of every environment value.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn authorized_launch_sees_the_client_environment_not_the_supervisors() {
    let client_path = OsString::from("/client/only/bin");
    assert_ne!(
        std::env::var_os("PATH"),
        Some(client_path.clone()),
        "the supervisor process must not already carry the client's PATH"
    );
    let raw_value = OsString::from_vec(vec![b'v', 0xff, b'=', 0xfe]);
    let context = request_context().with_environment([
        (OsString::from("PATH"), client_path.clone()),
        (
            OsString::from("MARION_NODE_TOKEN"),
            OsString::from("leaked"),
        ),
        (OsString::from("TERM_PROGRAM"), raw_value.clone()),
        (OsString::from("MARION_AGENT_ID"), OsString::from("leaked")),
    ]);
    assert_eq!(
        context_hash(&context),
        hash(),
        "environment values became hash inputs"
    );

    let handler = Arc::new(EnvironmentCapturingHandler {
        captured: Mutex::new(None),
    });
    let service = Arc::new(
        NativeBootstrapService::new_without_terminal_verification_for_task2_tests(
            context.canonical_project().to_path_buf(),
            context.native_wire_version(),
            Arc::clone(&handler) as Arc<dyn NativeBootstrapHandler>,
        ),
    );
    let (mut client, server) = UnixStream::pair().unwrap();
    let worker = {
        let service = Arc::clone(&service);
        let active = service.try_acquire_connection().unwrap();
        std::thread::spawn(move || service.serve_connection(ConnId(33), server, active))
    };
    let input = std::fs::File::open("/dev/null").unwrap();
    let output = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .unwrap();
    let capability =
        request_direct_cli_capability(&mut client, input.as_fd(), output.as_fd(), &context)
            .unwrap();
    present_direct_cli_capability(&mut client, capability, &context).unwrap();
    worker.join().unwrap();

    assert_eq!(
        lock(&handler.captured)
            .take()
            .expect("the handler was authorized"),
        vec![
            (OsString::from("PATH"), client_path),
            (OsString::from("TERM_PROGRAM"), raw_value),
        ],
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn received_rights_cannot_cross_the_real_spawn_boundary() {
    use crate::spawn_receive_gate::{SpawnTestEvent, spawn_test_hook};

    #[derive(Debug)]
    enum RaceSignal {
        Contended,
        HookClosed,
        Child(Option<i32>),
    }

    let scratch = marion_testsupport::scratch("spawn-receive-race");
    let path = scratch.join("rights");
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let identity = fstat(&file).unwrap();
    let (client, server) = UnixStream::pair().unwrap();
    send_terminal_descriptors(&client, file.as_fd(), file.as_fd()).unwrap();
    drop(file);

    let (receive_control, receive_install) = receive_test_hook();
    let receiver = std::thread::spawn(move || {
        let _hook = receive_install.install();
        receive_terminal_descriptors(&server)
    });
    let post_recv = receive_control.event();
    #[cfg(target_os = "linux")]
    assert_eq!(post_recv.cloexec, vec![true, true]);
    #[cfg(target_os = "macos")]
    assert_eq!(post_recv.cloexec, vec![false, false]);

    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "native_bootstrap::tests::received_rights_child_probe",
        ])
        .env("MARION_RECEIVED_RIGHTS_PROBE", "1")
        .env("MARION_RECEIVED_RIGHTS_DEV", identity.st_dev.to_string())
        .env("MARION_RECEIVED_RIGHTS_INO", identity.st_ino.to_string());
    let (spawn_control, spawn_install) = spawn_test_hook();
    let (signal_tx, signal_rx) = std::sync::mpsc::sync_channel(4);
    let spawn_observer = {
        let signal_tx = signal_tx.clone();
        std::thread::spawn(move || {
            loop {
                match spawn_control.event_result() {
                    Ok(SpawnTestEvent::Attempting) => {}
                    Ok(SpawnTestEvent::Contended) => {
                        signal_tx.send(RaceSignal::Contended).unwrap();
                    }
                    Ok(SpawnTestEvent::Acquired) => {
                        spawn_control.resume();
                        return;
                    }
                    Err(()) => {
                        signal_tx.send(RaceSignal::HookClosed).unwrap();
                        return;
                    }
                }
            }
        })
    };
    let child = {
        let signal_tx = signal_tx.clone();
        std::thread::spawn(move || {
            let _hook = spawn_install.install();
            let code = crate::run::run_bounded(&mut command, Duration::from_secs(5))
                .ok()
                .and_then(|output| output.code);
            signal_tx.send(RaceSignal::Child(code)).unwrap();
        })
    };
    drop(signal_tx);

    #[cfg(target_os = "macos")]
    assert!(matches!(
        signal_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        RaceSignal::Contended
    ));
    #[cfg(target_os = "macos")]
    receive_control.resume();

    let child_code = match signal_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
        RaceSignal::Child(code) => code,
        RaceSignal::Contended => panic!("Linux atomic receipt must not take the spawn gate"),
        RaceSignal::HookClosed => panic!("real run_bounded spawn bypassed the gate"),
    };
    assert_eq!(child_code, Some(0));

    #[cfg(target_os = "linux")]
    receive_control.resume();
    let received = receiver.join().unwrap().unwrap();
    for descriptor in &received {
        assert!(fcntl_getfd(descriptor).unwrap().contains(FdFlags::CLOEXEC));
    }
    child.join().unwrap();
    spawn_observer.join().unwrap();
    drop(client);
    let _ = std::fs::remove_file(path);
    drop(scratch);
}

#[cfg(target_os = "macos")]
#[test]
fn unwinding_before_ancillary_drain_closes_rights_before_spawn_can_enter() {
    use crate::spawn_receive_gate::{SpawnTestEvent, spawn_test_hook};

    let scratch = marion_testsupport::scratch("spawn-receive-unwind");
    let path = scratch.join("rights");
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let identity = fstat(&file).unwrap();
    let (client, server) = UnixStream::pair().unwrap();
    send_terminal_descriptors(&client, file.as_fd(), file.as_fd()).unwrap();
    drop(file);

    let (unwind_control, unwind_install) = receive_unwind_test_hook();
    let (receiver_tx, receiver_rx) = std::sync::mpsc::sync_channel(1);
    let receiver = std::thread::spawn(move || {
        let _hook = unwind_install.install();
        let panicked = std::panic::catch_unwind(|| receive_terminal_descriptors(&server)).is_err();
        receiver_tx.send(panicked).unwrap();
    });
    unwind_control
        .received
        .recv_timeout(Duration::from_secs(5))
        .unwrap();

    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "native_bootstrap::tests::received_rights_child_probe",
        ])
        .env("MARION_RECEIVED_RIGHTS_PROBE", "1")
        .env("MARION_RECEIVED_RIGHTS_DEV", identity.st_dev.to_string())
        .env("MARION_RECEIVED_RIGHTS_INO", identity.st_ino.to_string());
    let (spawn_control, spawn_install) = spawn_test_hook();
    let (child_tx, child_rx) = std::sync::mpsc::sync_channel(1);
    let child = std::thread::spawn(move || {
        let _hook = spawn_install.install();
        let code = crate::run::run_bounded(&mut command, Duration::from_secs(5))
            .ok()
            .and_then(|output| output.code);
        child_tx.send(code).unwrap();
    });
    assert_eq!(
        spawn_control.event_timeout(Duration::from_secs(5)),
        Ok(SpawnTestEvent::Attempting)
    );
    assert_eq!(
        spawn_control.event_timeout(Duration::from_secs(5)),
        Ok(SpawnTestEvent::Contended)
    );

    unwind_control.panic_now.send(()).unwrap();
    unwind_control
        .unwound
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert_eq!(
        spawn_control.event_timeout(Duration::from_secs(5)),
        Ok(SpawnTestEvent::Acquired)
    );
    spawn_control.resume();
    assert_eq!(
        child_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        Some(0)
    );
    unwind_control.resume_unwind.send(()).unwrap();
    assert!(receiver_rx.recv_timeout(Duration::from_secs(5)).unwrap());

    child.join().unwrap();
    receiver.join().unwrap();
    drop(client);
    let _ = std::fs::remove_file(path);
    drop(scratch);
}

#[cfg(target_os = "macos")]
#[test]
fn gate_contention_consumes_the_absolute_receive_deadline_before_recvmsg() {
    use crate::spawn_receive_gate::{SPAWN_RECEIVE_GATE, SpawnTestEvent, spawn_test_hook};

    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    let (holder_done_tx, holder_done_rx) = std::sync::mpsc::sync_channel(1);
    let holder = std::thread::spawn(move || {
        SPAWN_RECEIVE_GATE.receive_non_atomic(|| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        holder_done_tx.send(()).unwrap();
    });
    entered_rx.recv().unwrap();

    let clock = Arc::new(ManualClock::default());
    let deadline = HandshakeDeadline::new(clock.clone(), Duration::from_millis(25)).unwrap();
    let (client, server) = UnixStream::pair().unwrap();
    let (control, install) = spawn_test_hook();
    let (receiver_tx, receiver_rx) = std::sync::mpsc::sync_channel(1);
    let receiver = std::thread::spawn(move || {
        let _hook = install.install();
        receiver_tx
            .send(receive_terminal_descriptors_before(&server, &deadline))
            .unwrap();
    });
    let attempting = control.event_timeout(Duration::from_secs(1));
    let contended = control.event_timeout(Duration::from_secs(1));
    clock.set(Duration::from_millis(26));
    drop(client);
    let release = release_tx.send(());
    let acquired = control.event_timeout(Duration::from_secs(1));
    if acquired == Ok(SpawnTestEvent::Acquired) {
        control.resume();
    }
    let result = receiver_rx.recv_timeout(Duration::from_secs(2));
    let holder_done = holder_done_rx.recv_timeout(Duration::from_secs(2));
    if result.is_ok() {
        receiver.join().unwrap();
    }
    if holder_done.is_ok() {
        holder.join().unwrap();
    }

    assert_eq!(attempting, Ok(SpawnTestEvent::Attempting));
    assert_eq!(contended, Ok(SpawnTestEvent::Contended));
    assert_eq!(acquired, Ok(SpawnTestEvent::Acquired));
    assert!(release.is_ok());
    assert!(holder_done.is_ok());
    assert!(matches!(
        result.unwrap(),
        Err(BootstrapError::HandshakeExpired)
    ));
}

#[test]
fn received_rights_child_probe() {
    if std::env::var_os("MARION_RECEIVED_RIGHTS_PROBE").is_none() {
        return;
    }
    let expected_dev: u64 = std::env::var("MARION_RECEIVED_RIGHTS_DEV")
        .unwrap()
        .parse()
        .unwrap();
    let expected_ino: u64 = std::env::var("MARION_RECEIVED_RIGHTS_INO")
        .unwrap()
        .parse()
        .unwrap();
    let fd_dir = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    for entry in std::fs::read_dir(fd_dir).unwrap() {
        let Ok(fd) = entry.unwrap().file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if fd <= 2 {
            continue;
        }
        // SAFETY: the fd came from this process's live fd directory and is borrowed for one fstat.
        let descriptor = unsafe { BorrowedFd::borrow_raw(fd) };
        if let Ok(stat) = fstat(descriptor) {
            #[cfg(target_os = "macos")]
            let device = stat.st_dev as u64;
            #[cfg(target_os = "linux")]
            let device = stat.st_dev;
            if device == expected_dev && stat.st_ino == expected_ino {
                panic!("spawned child inherited received descriptor {fd}");
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod supported_descriptors {
    use super::*;
    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};
    use std::io::IoSlice;
    use std::mem::MaybeUninit;

    fn send_count(stream: &UnixStream, files: &[std::fs::File]) {
        if files.is_empty() {
            (&*stream).write_all(DESCRIPTOR_MESSAGE).unwrap();
            return;
        }
        let fds: Vec<_> = files.iter().map(AsFd::as_fd).collect();
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        assert!(control.push(SendAncillaryMessage::ScmRights(&fds)));
        assert_eq!(
            sendmsg(
                stream,
                &[IoSlice::new(DESCRIPTOR_MESSAGE)],
                &mut control,
                SendFlags::empty()
            )
            .unwrap(),
            DESCRIPTOR_MESSAGE.len()
        );
    }

    #[test]
    fn forged_context_hash_is_refused_before_authority_lookup_or_downstream_effects() {
        let effects = Arc::new(Effects::default());
        let handler = Arc::new(CountingHandler {
            terminal_verifications: AtomicU64::new(0),
            effects: Arc::clone(&effects),
        });
        let service = Arc::new(
            NativeBootstrapService::new_without_terminal_verification_for_task2_tests(
                request_context().canonical_project().to_path_buf(),
                request_context().native_wire_version(),
                Arc::clone(&handler) as Arc<dyn NativeBootstrapHandler>,
            ),
        );
        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = {
            let service = Arc::clone(&service);
            let active = service.try_acquire_connection().unwrap();
            std::thread::spawn(move || service.serve_connection(ConnId(39), server, active))
        };
        let input = std::fs::File::open("/dev/null").unwrap();
        let output = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap();
        send_terminal_descriptors(&client, input.as_fd(), output.as_fd()).unwrap();
        write_wire_request(
            &mut client,
            &WireRequest::Issue {
                context: request_context(),
                claimed_hash: ContextHash([0x44; 32]),
            },
        )
        .unwrap();
        let mut status = [WIRE_OK];
        client.read_exact(&mut status).unwrap();
        assert_eq!(status, [WIRE_REFUSED]);
        worker.join().unwrap();
        assert_eq!(handler.terminal_verifications.load(Ordering::SeqCst), 0);
        assert_eq!(service.capability_lookup_count(), 0);
        effects.assert_zero();
    }

    #[test]
    fn wrong_project_and_version_are_refused_before_terminal_detail_or_authority() {
        let mutations: [fn(&mut DirectNativeRequestContext); 2] = [
            |context: &mut DirectNativeRequestContext| {
                context.canonical_project = PathBuf::from("/wrong")
            },
            |context: &mut DirectNativeRequestContext| context.native_wire_version += 1,
        ];
        for mutate in mutations {
            let effects = Arc::new(Effects::default());
            let handler = Arc::new(CountingHandler {
                terminal_verifications: AtomicU64::new(0),
                effects: Arc::clone(&effects),
            });
            let expected = request_context();
            let service = Arc::new(
                NativeBootstrapService::new_without_terminal_verification_for_task2_tests(
                    expected.canonical_project().to_path_buf(),
                    expected.native_wire_version(),
                    Arc::clone(&handler) as Arc<dyn NativeBootstrapHandler>,
                ),
            );
            let (mut client, server) = UnixStream::pair().unwrap();
            let worker = {
                let active = service.try_acquire_connection().unwrap();
                let service = Arc::clone(&service);
                std::thread::spawn(move || service.serve_connection(ConnId(41), server, active))
            };
            let input = std::fs::File::open("/dev/null").unwrap();
            let output = std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/null")
                .unwrap();
            send_terminal_descriptors(&client, input.as_fd(), output.as_fd()).unwrap();
            let mut context = expected;
            mutate(&mut context);
            write_wire_request(
                &mut client,
                &WireRequest::Issue {
                    claimed_hash: context_hash(&context),
                    context,
                },
            )
            .unwrap();
            let mut status = [WIRE_OK];
            client.read_exact(&mut status).unwrap();
            assert_eq!(status, [WIRE_REFUSED]);
            worker.join().unwrap();
            assert_eq!(handler.terminal_verifications.load(Ordering::SeqCst), 0);
            assert_eq!(service.capability_lookup_count(), 0);
            effects.assert_zero();
        }
    }

    #[test]
    fn ancillary_count_matrix_ctrunc_and_descriptor_roles_are_capability_based() {
        for count in [0, 1, 3, 4] {
            let (client, server) = UnixStream::pair().unwrap();
            let files: Vec<_> = (0..count)
                .map(|_| std::fs::File::open("/dev/null").unwrap())
                .collect();
            send_count(&client, &files);
            let error = receive_terminal_descriptors(&server).unwrap_err();
            if count == 4 {
                assert!(
                    matches!(error, BootstrapError::AncillaryTruncated),
                    "unexpected four-descriptor result: {error:?}"
                );
            } else {
                assert!(matches!(error, BootstrapError::DescriptorCount));
            }
        }

        let (client, server) = UnixStream::pair().unwrap();
        let read = std::fs::File::open("/dev/null").unwrap();
        let write = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap();
        send_terminal_descriptors(&client, write.as_fd(), read.as_fd()).unwrap();
        let connection = NativeBootstrapConnection::authenticate(ConnId(40), server).unwrap();
        assert!(matches!(
            connection.receive_terminal(geometry()),
            Err(BootstrapError::DescriptorRoles)
        ));

        let (client, server) = UnixStream::pair().unwrap();
        let both = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        send_terminal_descriptors(&client, both.as_fd(), both.as_fd()).unwrap();
        let connection = NativeBootstrapConnection::authenticate(ConnId(41), server).unwrap();
        connection
            .receive_terminal(geometry())
            .expect("O_RDWR supplies both read and write capabilities");
    }

    #[test]
    fn every_received_descriptor_is_closed_when_ancillary_data_is_truncated() {
        let (client, server) = UnixStream::pair().unwrap();
        let pipes: Vec<_> = (0..4).map(|_| rustix::pipe::pipe().unwrap()).collect();
        let writes: Vec<_> = pipes.iter().map(|(_, write)| write.as_fd()).collect();
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        assert!(control.push(SendAncillaryMessage::ScmRights(&writes)));
        sendmsg(
            &client,
            &[IoSlice::new(DESCRIPTOR_MESSAGE)],
            &mut control,
            SendFlags::empty(),
        )
        .unwrap();
        let reads: Vec<_> = pipes.into_iter().map(|(read, _)| read).collect();
        let result = receive_terminal_descriptors(&server);
        assert!(
            matches!(result, Err(BootstrapError::AncillaryTruncated)),
            "unexpected truncation result: {result:?}"
        );
        for read in reads {
            assert_eq!(rustix::io::read(&read, &mut [0u8; 1]).unwrap(), 0);
        }
    }

    #[test]
    fn exact_pair_is_cloexec_and_different_identities_refuse() {
        let (client, server) = UnixStream::pair().unwrap();
        let read = std::fs::File::open("/dev/null").unwrap();
        let write = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap();
        send_terminal_descriptors(&client, read.as_fd(), write.as_fd()).unwrap();
        let [stdin, stdout] = receive_terminal_descriptors(&server).unwrap();
        assert!(
            rustix::io::fcntl_getfd(&stdin)
                .unwrap()
                .contains(FdFlags::CLOEXEC)
        );
        assert!(
            rustix::io::fcntl_getfd(&stdout)
                .unwrap()
                .contains(FdFlags::CLOEXEC)
        );

        let temp_path =
            std::env::temp_dir().join(format!("marion-native-bootstrap-fd-{}", std::process::id()));
        let temp = std::fs::File::create(&temp_path).unwrap();
        let (client, server) = UnixStream::pair().unwrap();
        send_terminal_descriptors(&client, read.as_fd(), temp.as_fd()).unwrap();
        let connection = NativeBootstrapConnection::authenticate(ConnId(42), server).unwrap();
        assert!(matches!(
            connection.receive_terminal(geometry()),
            Err(BootstrapError::DifferentTerminals)
        ));
        let _ = std::fs::remove_file(temp_path);
    }
}
