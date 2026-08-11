use super::*;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::OsStringExt;
use std::sync::Barrier;

#[derive(Default)]
struct ManualClock(AtomicU64);

impl ManualClock {
    fn set(&self, now: Duration) {
        self.0.store(now.as_nanos() as u64, Ordering::SeqCst);
    }
}

impl MonotonicClock for ManualClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.0.load(Ordering::SeqCst))
    }
}

#[derive(Default)]
struct SequenceRng(AtomicU64);

impl CapabilityRng for SequenceRng {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), BootstrapError> {
        let next = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        bytes.fill(0);
        bytes[..8].copy_from_slice(&next.to_be_bytes());
        Ok(())
    }
}

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
        _stdin: stdin,
        _stdout: stdout,
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

fn request_context() -> DirectNativeRequestContext {
    DirectNativeRequestContext::new(
        PathBuf::from(OsString::from_vec(b"/project-\xff".to_vec())),
        OsString::from("atlas"),
        vec![OsString::from_vec(vec![b'a', 0xfe]), OsString::new()],
        OsString::from("xterm-256color"),
        7,
    )
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
fn context_hash_has_a_fixed_vector_and_every_semantic_boundary_is_load_bearing() {
    let base = DirectNativeRequestContext::new(
        PathBuf::from(OsString::from_vec(b"/project-\xff".to_vec())),
        OsString::from("atlas"),
        vec![OsString::from_vec(vec![b'a', 0xfe]), OsString::new()],
        OsString::from("xterm-256color"),
        7,
    );
    assert_eq!(
        context_hash(&base).0,
        [
            100, 208, 119, 25, 123, 215, 168, 248, 160, 18, 90, 119, 53, 179, 18, 216, 227, 150,
            157, 8, 55, 78, 242, 24, 14, 101, 243, 103, 90, 5, 234, 230,
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
    bytes
}

fn budget_context(argument_bytes: usize) -> DirectNativeRequestContext {
    DirectNativeRequestContext::new(
        PathBuf::from(OsString::from_vec(vec![b'p'; MAX_CONTEXT_FIELD_BYTES])),
        OsString::from("s"),
        vec![OsString::from_vec(vec![b'a'; argument_bytes])],
        OsString::from_vec(vec![b't'; MAX_CONTEXT_FIELD_BYTES]),
        NATIVE_WIRE_VERSION,
    )
}

#[test]
fn aggregate_context_budget_accepts_exact_boundary_and_rejects_cumulative_and_checked_overflow() {
    let exact = budget_context(MAX_CONTEXT_FIELD_BYTES - 25);
    assert_eq!(
        checked_context_wire_size(&exact).unwrap(),
        MAX_CONTEXT_BYTES
    );
    let mut encoded = std::io::Cursor::new(encoded_context(&exact));
    assert_eq!(read_context(&mut encoded).unwrap(), exact);

    let over = budget_context(MAX_CONTEXT_FIELD_BYTES - 24);
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
fn service_policy_rejects_wrong_project_and_wire_version() {
    let service = NativeBootstrapService::disabled(PathBuf::from("/expected"));
    let wrong_project = DirectNativeRequestContext::new(
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

    fn authorized(&self, request: ConsumedNativeRequest<'_>) -> Result<(), BootstrapError> {
        assert_eq!(request.hash(), context_hash(request.context()));
        self.effects.record_authorized(request.authorization());
        Ok(())
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
    let service = Arc::new(NativeBootstrapService::new(
        request_context().canonical_project().to_path_buf(),
        request_context().native_wire_version(),
        Arc::clone(&handler) as Arc<dyn NativeBootstrapHandler>,
    ));
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
    let service = Arc::new(NativeBootstrapService::new(
        request_context().canonical_project().to_path_buf(),
        request_context().native_wire_version(),
        Arc::clone(&handler) as Arc<dyn NativeBootstrapHandler>,
    ));
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
    present_direct_cli_capability(&mut client, capability, &context).unwrap();
    worker.join().unwrap();
    assert_eq!(handler.terminal_verifications.load(Ordering::SeqCst), 1);
    assert_eq!(effects.descriptor_details.load(Ordering::SeqCst), 1);
    assert_eq!(effects.filesystem.load(Ordering::SeqCst), 1);
    assert_eq!(effects.processes.load(Ordering::SeqCst), 1);
    assert_eq!(effects.artifacts.load(Ordering::SeqCst), 1);
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
        let service = Arc::new(NativeBootstrapService::new(
            request_context().canonical_project().to_path_buf(),
            request_context().native_wire_version(),
            Arc::clone(&handler) as Arc<dyn NativeBootstrapHandler>,
        ));
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
            let service = Arc::new(NativeBootstrapService::new(
                expected.canonical_project().to_path_buf(),
                expected.native_wire_version(),
                Arc::clone(&handler) as Arc<dyn NativeBootstrapHandler>,
            ));
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
