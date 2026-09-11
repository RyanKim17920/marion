//! Tests for the pty host.
//!
//! Every one names the mutation it exists to catch, and none of them can fail only by timing out:
//! a test whose red state is "the suite hung" is a test nobody will run twice. Where a real process
//! is involved the wait is bounded by [`until`] and the failure is an assertion, with `kill(pid, 0)`
//! consulted first so the message says whether the child was alive or the event never came.

use super::*;
use marion_harness::{ControlTransport, ExecutionSurfaces, TypedKind};
use marion_testsupport::until;
use std::io::Read;
use std::time::Duration;

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

fn bounded_hook_gate() -> (
    Box<dyn Fn() + Send + Sync>,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::SyncSender<()>,
) {
    let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    let hook = Box::new(move || {
        reached_tx
            .send(())
            .expect("the deterministic assertion side is alive");
        release_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(Duration::from_secs(2))
            .expect("the deterministic hook was not released");
    });
    (hook, reached_rx, release_tx)
}

unsafe extern "C" {
    fn kill(pid: c_int, sig: c_int) -> c_int;
    /// The session id of the session for which this terminal is the **controlling terminal**.
    /// `-1`/`ENOTTY` if it is nobody's, which is exactly the state dropping `TIOCSCTTY` produces.
    fn tcgetsid(fd: c_int) -> c_int;
    /// The foreground process group of that session.
    fn tcgetpgrp(fd: c_int) -> c_int;
}

/// Does a process exist? `kill(pid, 0)` asks without sending anything.
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs the permission and existence check and delivers nothing.
    unsafe { kill(pid, 0) == 0 }
}

fn witness() -> PtyWitness {
    ExecutionSurfaces::opaque()
        .display_plane()
        .expect("`opaque` is a pty surface")
}

fn sh(script: &str) -> Command {
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(script);
    c.env("TERM", "xterm-256color");
    c
}

/// Read `pty.cast` back as (code, data) pairs, plus the header.
fn read_cast(path: &Path) -> (serde_json::Value, Vec<(f64, String, String)>) {
    let text = std::fs::read_to_string(path).expect("pty.cast exists");
    let mut lines = text.lines();
    let header: serde_json::Value =
        serde_json::from_str(lines.next().expect("a header line")).expect("header is JSON");
    let records = lines
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str::<(f64, String, String)>(l).expect("a [interval, code, data]")
        })
        .collect();
    (header, records)
}

fn read_stream(path: &Path) -> super::stream::SessionRecovery {
    let stream_path = super::stream::stream_path_for_cast(path).expect("a stream path");
    let encoded = std::fs::read(stream_path).expect("pty.stream exists");
    super::stream::recover_session_bytes(&encoded).expect("the incremental stream recovers")
}

/// A host with **no child**, whose slave the test drives by hand.
///
/// Most of what is asserted here is about ordering and encoding, not about any harness, and a real
/// process would make those tests probabilistic for nothing. The slave is a real pts either way, so
/// the kernel path under test is the same one production takes.
struct Loopback {
    _dir: marion_testsupport::Scratch,
    cast: PathBuf,
    host: PtyHost,
    slave: Option<std::fs::File>,
}

impl Loopback {
    fn new(tag: &str, size: WinSize) -> Self {
        Self::with_splice_limits(tag, size, super::splice::SpliceLimits::production())
    }

    fn with_splice_limits(tag: &str, size: WinSize, limits: super::splice::SpliceLimits) -> Self {
        let dir = marion_testsupport::scratch(tag);
        let cast = dir.join("pty.cast");
        let master = PtyMaster::open(size).expect("a pty");
        let slave = std::fs::File::from(master.open_slave().expect("the slave opens"));
        let host = PtyHost::start_with_splice_limits(
            AgentId("node-under-test".into()),
            master,
            &cast,
            size,
            "xterm-256color",
            Instant::now(),
            limits,
        )
        .expect("the host starts");
        Self {
            _dir: dir,
            cast,
            host,
            slave: Some(slave),
        }
    }

    fn without_reader(tag: &str, size: WinSize) -> Self {
        let dir = marion_testsupport::scratch(tag);
        let cast = dir.join("pty.cast");
        let master = PtyMaster::open(size).expect("a pty");
        let slave = std::fs::File::from(master.open_slave().expect("the slave opens"));
        let host = PtyHost::start_without_reader_for_test(
            AgentId("node-under-test".into()),
            master,
            &cast,
            size,
            "xterm-256color",
            Instant::now(),
        )
        .expect("the host starts without a reader");
        Self {
            _dir: dir,
            cast,
            host,
            slave: Some(slave),
        }
    }

    /// Write bytes as if the child had, and wait until the host has read them.
    fn child_writes(&mut self, bytes: &[u8]) {
        let before = self.host.bytes_read();
        self.slave
            .as_mut()
            .expect("still attached")
            .write_all(bytes)
            .expect("the slave accepts a write");
        assert!(
            until(|| self.host.bytes_read() >= before + bytes.len() as u64),
            "the host never read the {} bytes the slave wrote",
            bytes.len()
        );
    }

    /// Close the slave, so the reader thread reaches EOF and `shutdown` can join it.
    fn hang_up(&mut self) {
        self.slave.take();
    }
}

/// A replay must describe the geometry in which its first output was produced without relying on
/// the attach-time master size. The host therefore synthesizes one cursor-prefix Resize before its
/// bounded retained records, and that record becomes dense wire sequence zero.
#[test]
fn pane_replay_starts_with_the_initial_geometry() {
    let mut lb = Loopback::without_reader("pty-pane-initial-geometry", WinSize::new(80, 24));
    assert!(lb.host.retention_snapshot().is_empty());

    let conn = crate::serve::ConnId(1_200);
    let (out, captured) = crate::serve::capture(conn);
    let descriptor = lb
        .host
        .begin_pane_replay(conn, out)
        .expect("the initial geometry is replayable");
    assert_eq!(descriptor.cut, 1);
    lb.host.pane_ready(conn, &descriptor.token, descriptor.cut);

    let line = captured
        .try_recv()
        .expect("the initial Resize is delivered");
    let frame = marion_core::proto::Frame::from_line(std::str::from_utf8(&line).unwrap()).unwrap();
    let marion_core::proto::Frame::Notification(note) = frame else {
        panic!("the replay emitted a non-notification")
    };
    let marion_core::proto::Event::NodePaneFrame(frame) = note.event else {
        panic!("the replay emitted a non-pane event")
    };
    assert_eq!(frame.seq, 0);
    assert_eq!(
        frame.frame,
        marion_core::proto::PaneFrameKindV1::Resize { cols: 80, rows: 24 }
    );
    lb.host.unlisten(conn);
    lb.hang_up();
    lb.host.shutdown().unwrap();
}

/// Opaque input belongs to the negotiated pane slot, not to replay readiness. The exact valid,
/// generation-matching slot remains writable while Pending, during the response-first handoff's
/// Transitioning phase, and after it reaches Ready. Removing any phase from the admission match
/// must reject that byte and fail this lifecycle at the phase named by the assertion.
#[test]
fn opaque_input_is_admitted_through_every_valid_pane_slot_phase() {
    let mut lb = Loopback::without_reader("pty-pane-input-all-phases", WinSize::new(80, 24));
    let conn = crate::serve::ConnId(1_201);
    let lease = lb.host.lease_writer(conn).expect("the exact writer lease");
    let (out, captured) = crate::serve::capture(conn);
    let descriptor = lb
        .host
        .begin_pane_replay(conn, out.clone())
        .expect("the pane-v1 slot is reserved");

    let assert_phase = |expected: &str| {
        let streams = lb
            .host
            .shared
            .pane_streams
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let slot = streams
            .slots
            .get(&conn)
            .expect("the exact slot remains live");
        assert!(streams.valid);
        assert_eq!(slot.generation, streams.generation);
        assert!(!slot.cancellation.is_cancelled());
        assert!(
            matches!(
                (&slot.phase, expected),
                (PanePhase::Pending(_), "Pending")
                    | (PanePhase::Transitioning { .. }, "Transitioning")
                    | (PanePhase::Ready(_), "Ready")
            ),
            "expected {expected} pane slot"
        );
    };

    assert_phase("Pending");
    let pending = lb
        .host
        .admit_opaque_input(&lease, conn)
        .expect("Pending pane-v1 input is admissible");
    lb.host
        .write_opaque_input_admitted(pending, b"P")
        .expect("Pending pane-v1 input is delivered");

    let (hook, reached, release) = bounded_hook_gate();
    *lb.host
        .shared
        .pane_transition_hook
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(hook);
    lb.host.pane_ready(conn, &descriptor.token, descriptor.cut);
    let (captured, transitioning_result) = std::thread::scope(|scope| {
        let driver = scope.spawn(move || {
            let frames = captured.try_iter().collect::<Vec<_>>();
            (captured, frames)
        });
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("replay reached Transitioning");
        assert_phase("Transitioning");
        let result = lb
            .host
            .admit_opaque_input(&lease, conn)
            .and_then(|admission| lb.host.write_opaque_input_admitted(admission, b"T"));
        release.send(()).expect("the replay driver remains alive");
        let (captured, _frames) = driver.join().unwrap();
        (captured, result)
    });
    *lb.host
        .shared
        .pane_transition_hook
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = None;
    transitioning_result.expect("Transitioning pane-v1 input is admissible and delivered");

    assert_phase("Ready");
    let ready = lb
        .host
        .admit_opaque_input(&lease, conn)
        .expect("Ready pane-v1 input is admissible");
    lb.host
        .write_opaque_input_admitted(ready, b"R")
        .expect("Ready pane-v1 input is delivered");
    assert_eq!(out.departed(), None);

    lb.host.unlisten(conn);
    drop(captured);
    drop(lease);
    lb.hang_up();
    lb.host.shutdown().unwrap();
}

/// Initial geometry is cursor metadata, not one of the caller-configured retained record slots.
/// A one-record stream can still retain one Output before the next record reaches its bound.
#[test]
fn initial_geometry_does_not_consume_retained_record_capacity() {
    let mut lb = Loopback::with_splice_limits(
        "pty-pane-initial-geometry-capacity",
        WinSize::new(80, 24),
        super::splice::SpliceLimits::testing(16, 1),
    );

    lb.host.shared.retain_output(b"one");

    assert!(lb.host.retention_error().is_none());
    assert_eq!(lb.host.retention_snapshot().len(), 1);
    assert!(matches!(
        &lb.host.retention_snapshot()[0].kind,
        super::splice::DisplayKind::Output(bytes) if bytes.as_ref() == b"one"
    ));
    lb.hang_up();
    lb.host.shutdown().unwrap();
}

// ---------------------------------------------------------------------------------------------
// S11 MUST #2: two holes, two closures
// ---------------------------------------------------------------------------------------------

/// **The hole the witness does not close.**
///
/// `shared` is `Typed(StreamJson)` **and** `NativePty`, so it yields a `PtyWitness` and must still
/// speak stream-json over pipes — a witness-gated launcher would hand it the slave and `claude -p`
/// would exit 1 on `isatty(0)`. The match in [`stdin_plan`] is total and has no `_` arm, so a
/// fourth `ControlTransport` cannot compile until somebody has decided; this asserts the three that
/// exist.
///
/// **Mutation:** hand the slave to `Typed` (`ControlTransport::Typed(_) => StdinPlan::TerminalSlave`).
#[test]
fn every_control_transport_states_its_stdin() {
    // Exhaustive by construction: one entry per variant, and `TypedKind` is covered on the arm
    // where the kind could plausibly matter.
    let cases = [
        (
            ControlTransport::Typed(TypedKind::StreamJson),
            StdinPlan::Piped,
        ),
        (
            ControlTransport::Typed(TypedKind::AppServer),
            StdinPlan::Piped,
        ),
        (ControlTransport::Typed(TypedKind::Acp), StdinPlan::Piped),
        (ControlTransport::TerminalInput, StdinPlan::TerminalSlave),
        (ControlTransport::LaunchOnly, StdinPlan::Null),
    ];
    for (control, want) in cases {
        assert_eq!(stdin_plan(control), want, "{control:?}");
    }

    // And the preset that is the actual trap: it has a display plane *and* typed control.
    let shared = ExecutionSurfaces::shared(TypedKind::StreamJson);
    assert!(
        shared.display_plane().is_some(),
        "`shared` really does own a pty — that is what makes it the counterexample"
    );
    assert_eq!(
        stdin_plan(shared.control),
        StdinPlan::Piped,
        "§6.4: a node speaking stream-json gets pipes on stdin even though marion owns its pty"
    );
}

// ---------------------------------------------------------------------------------------------
// the fd topology
// ---------------------------------------------------------------------------------------------

/// `O_CLOEXEC` is load-bearing (topology point 1) and POSIX does not require `posix_openpt` to
/// honour it, so it is read back from the kernel rather than assumed. `TIOCSWINSZ` is round-tripped
/// through `TIOCGWINSZ`, which is what turns two hand-written `ioctl` numbers into a measurement.
///
/// **Mutation:** drop `O_CLOEXEC` from the open flags; transpose the `TIOCSWINSZ` constant's digits.
#[test]
fn the_master_is_close_on_exec_and_its_size_round_trips() {
    let m = PtyMaster::open(WinSize::new(120, 40)).expect("a pty");
    assert_eq!(m.intended_size(), WinSize::new(120, 40));
    // Topology point 3: the kernel will not accept a size until a slave exists, so `open_slave` is
    // where the held value lands. Measured on Darwin 25.5.0 — `TIOCSWINSZ` before then is `ENOTTY`.
    let slave = m.open_slave().expect("a slave");
    assert!(
        m.is_cloexec(),
        "a master inherited past exec means the supervisor's read never reaches EOF"
    );
    assert_eq!(m.size().unwrap(), WinSize::new(120, 40));
    m.set_size(WinSize::new(100, 24)).unwrap();
    assert_eq!(
        m.size().unwrap(),
        WinSize::new(100, 24),
        "cols and rows must not have been transposed on the way through `struct winsize`"
    );
    assert!(m.slave_path().starts_with("/dev/"), "{:?}", m.slave_path());
    drop(slave);
}

/// Topology points 5 and 3: `setsid` then `TIOCSCTTY`, so the child is a session leader whose
/// controlling terminal is *this* pty. `sid == pgid == pid` is what proves `setsid` ran; a `pts`
/// device from `tty` is what proves the slave became stdin.
///
/// **Mutation:** drop `setsid` — `TIOCSCTTY` then fails (only a session leader may claim a
/// controlling terminal), `pre_exec` returns the error, and the spawn is refused.
///
/// **This test does not witness `TIOCSCTTY`, and the reason is a confound rather than a platform
/// fact.** It used to say the opposite — that Darwin claims the controlling terminal on `setsid()`
/// alone whenever the slave is on fd 0, so the ioctl was redundant here and its deletion
/// unobservable. That is measured false (S19, `tests/fixtures/s19/README.md`): `setsid()` alone
/// leaves `tcgetsid(master)` at `ENOTTY` in all nine cells.
///
/// What actually happens is that **this test's child is a shell**, and macOS's `/bin/sh` claims the
/// controlling terminal itself when it starts as a session leader without one and its stdin is a
/// tty. So the `tcgetsid` assertion below is satisfied by `sh` and survives deleting the ioctl —
/// which is exactly how a real assertion comes to look vacuous. Every test in this file drives its
/// child through [`sh`], so all of them inherit it.
///
/// The ioctl is pinned by [`tiocsctty_and_not_setsid_is_what_claims_the_terminal`], whose child is
/// `/bin/sleep` execed directly. **Do not "simplify" that test to use [`sh`]** — the absence of a
/// shell is the entire measurement.
#[test]
fn the_child_is_a_session_leader_with_the_master_as_its_controlling_terminal() {
    let dir = marion_testsupport::scratch("pty-sid");
    let master = PtyMaster::open(WinSize::new(80, 24)).expect("a pty");
    let slave_path = master.slave_path().to_path_buf();
    let master_fd = master.as_raw();
    let host = PtyHost::start(
        AgentId("sid".into()),
        master,
        &dir.join("pty.cast"),
        WinSize::new(80, 24),
        "xterm-256color",
        Instant::now(),
    )
    .unwrap();

    let seen_pid = std::cell::Cell::new(None);
    let child = spawn_pty(
        witness(),
        &mut sh("tty; sleep 5"),
        host.master(),
        StdinPlan::TerminalSlave,
        Some(&|pid| seen_pid.set(Some(pid))),
    )
    .expect("the child spawns");
    let pid = child.pid();
    assert_eq!(
        seen_pid.get(),
        Some(pid),
        "topology point 7: `on_started` fires between spawn and the first byte"
    );
    host.adopt(child);

    // The kernel is asked directly, rather than `ps` — macOS's `ps` has no `sid` keyword, and
    // `tcgetsid` is the sharper question anyway: it is answerable **only** if this pty is some
    // session's controlling terminal, which is precisely what `TIOCSCTTY` establishes.
    assert!(
        until(|| {
            // SAFETY: `master_fd` is open for as long as `host` is, which is this whole function.
            (unsafe { tcgetsid(master_fd) }) == pid
        }),
        "the pty is not the controlling terminal of session {pid}: tcgetsid says {} (errno {:?}). \
         Dropping `setsid` leaves the child in marion's session; dropping `TIOCSCTTY` leaves the \
         pty nobody's controlling terminal.",
        unsafe { tcgetsid(master_fd) },
        io::Error::last_os_error().raw_os_error()
    );
    assert_eq!(
        // SAFETY: as above.
        unsafe { tcgetpgrp(master_fd) },
        pid,
        "sid == pgid == pid is what `setsid` produces, and it is what makes the child's whole \
         group addressable by `killpg`"
    );

    // And the child agrees about which terminal it is on.
    assert!(
        until(|| {
            let (_, records) = read_cast(&dir.join("pty.cast"));
            records
                .iter()
                .any(|(_, c, d)| c == "o" && d.contains(slave_path.to_str().unwrap()))
        }),
        "`tty` never named {} — the child's stdin is some other terminal",
        slave_path.display()
    );

    host.shutdown().unwrap();
    assert!(until(|| !alive(pid)));
}

/// **A `shared` node — pipe on stdin, pty on stdout/stderr — still gets the pty as its controlling
/// terminal.**
///
/// This is the topology the brief's `ioctl(0, TIOCSCTTY, 0)` gets wrong: fd 0 is a pipe there, the
/// ioctl answers `ENOTTY`, `pre_exec` returns an error and the spawn fails outright — for the one
/// preset that has both a typed control plane and a display plane.
///
/// This used to claim it was the **only** topology on macOS in which `TIOCSCTTY` is observable,
/// because the fd-0 case supposedly got its controlling terminal from `setsid()` alone. Measured
/// false — S19, `tests/fixtures/s19/README.md`: the ioctl is what claims the terminal in *both*
/// topologies, and the earlier probe's `setsid`-only cell was measuring a shell. This test is still
/// where the **wrong-descriptor** mutation goes red, which is the distinct thing it is for.
///
/// **Mutation:** issue the ioctl on fd 0 regardless of the stdin plan, which is the shape §5.3
/// specifies — the spawn then fails outright with `ENOTTY`. (Deleting the ioctl altogether is also
/// caught here, and by [`tiocsctty_and_not_setsid_is_what_claims_the_terminal`], which is the one
/// that isolates it from the descriptor question.)
#[test]
fn a_piped_stdin_node_still_gets_the_pty_as_its_controlling_terminal() {
    let shared = ExecutionSurfaces::shared(TypedKind::StreamJson);
    let plan = stdin_plan(shared.control);
    assert_eq!(plan, StdinPlan::Piped, "the preset under test");

    let master = PtyMaster::open(WinSize::new(80, 24)).unwrap();
    let master_fd = master.as_raw();
    let mut cmd = sh("sleep 5");
    let mut child = spawn_pty(
        shared.display_plane().expect("`shared` owns a pty"),
        &mut cmd,
        &master,
        plan,
        None,
    )
    .expect("a shared node spawns: `ioctl(0, TIOCSCTTY)` here would be ENOTTY and refuse");
    let pid = child.pid();
    assert!(
        child.take_stdin().is_some(),
        "the typed control plane needs the write half of that pipe"
    );
    assert!(
        until(|| (unsafe { tcgetsid(master_fd) }) == pid),
        "the pty is nobody's controlling terminal: tcgetsid says {}. With stdin a pipe, `setsid` \
         alone does not claim it — the ioctl must name the descriptor the slave is actually on.",
        unsafe { tcgetsid(master_fd) }
    );
    crate::run::kill_process_tree(pid);
    let _ = child.wait();
}

/// Topology point 6, checked **structurally** rather than by waiting.
///
/// The observable consequence of forgetting it is a `read` that never returns, and a test whose red
/// state is a hang is not a test. `Stdio::from(OwnedFd)` parks the descriptor on the `Command`,
/// which outlives `spawn`, so the fix is to replace the three slots afterwards — and whether that
/// happened is answerable in microseconds by asking the kernel whether those descriptor numbers are
/// still open.
///
/// **Mutation:** delete the `command.stdin(null).stdout(null).stderr(null)` reset in `spawn_pty`.
#[test]
fn the_parent_closes_its_own_slave_copies() {
    let master = PtyMaster::open(WinSize::new(80, 24)).unwrap();
    // **The `Command` is held in a binding on purpose.** Passing `&mut sh("exit 0")` would let the
    // temporary drop at the end of the statement, which closes the parked `Stdio`s for free — and
    // the test would then pass with the reset deleted, measuring the temporary's drop instead of
    // the code under test.
    let mut cmd = sh("exit 0");
    let mut child =
        spawn_pty(witness(), &mut cmd, &master, StdinPlan::TerminalSlave, None).unwrap();
    for fd in child.handed_fds() {
        assert!(fd >= 0, "all three streams were handed a slave dup");
        // SAFETY: `F_GETFD` only reads. The descriptor is expected to be closed, which is exactly
        // what makes this call safe: it can only answer `EBADF`.
        let rc = unsafe { fcntl(fd, F_GETFD) };
        assert_eq!(
            rc, -1,
            "fd {fd} is still open in the parent, so the master will never see EOF"
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(9),
            "EBADF, i.e. closed — not some other failure"
        );
    }
    let _ = child.wait();
    drop(cmd);
}

/// Polling for a pane exit must leave the leader waitable. The zombie pins its pid/process group
/// until teardown has swept descendants, and the eventual `wait` must still return the child's
/// exact status rather than a synthesized readiness bit.
#[test]
fn poll_exited_unreaped_preserves_the_exact_wait_status() {
    let master = PtyMaster::open(WinSize::new(80, 24)).unwrap();
    let mut child = spawn_pty(
        witness(),
        &mut sh("exit 37"),
        &master,
        StdinPlan::TerminalSlave,
        None,
    )
    .unwrap();

    assert!(
        until(|| child
            .poll_exited_unreaped()
            .expect("non-consuming exit poll")),
        "the exited child was never observed"
    );
    assert_eq!(
        child
            .wait()
            .expect("the observed zombie remains waitable")
            .code(),
        Some(37),
        "non-consuming observation must preserve the exact wait status"
    );
}

/// A naturally exited pane leader must remain waitable until shutdown has signalled its process
/// group. The background holder ignores HUP and inherits the slave, so reaping the leader during
/// the poll strands both the holder and the reader; a non-consuming poll lets shutdown kill the
/// still-identifiable group, preserve exit 37, observe real EOF, and retain one terminal End.
#[test]
fn unreaped_leader_keeps_the_same_pgid_slave_holder_sweepable() {
    const MARKER_LIMIT: u64 = 128;

    fn read_marker(path: &Path) -> io::Result<String> {
        let file = std::fs::File::open(path)?;
        let mut value = String::new();
        file.take(MARKER_LIMIT).read_to_string(&mut value)?;
        Ok(value)
    }

    struct KillCandidatesOnDrop {
        pid_files: [PathBuf; 2],
        armed: bool,
    }

    impl KillCandidatesOnDrop {
        fn new(holder: &Path, spawned: &Path) -> Self {
            Self {
                pid_files: [holder.to_owned(), spawned.to_owned()],
                armed: true,
            }
        }

        fn disarm(&mut self) {
            self.armed = false;
        }
    }

    impl Drop for KillCandidatesOnDrop {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            for path in &self.pid_files {
                let Ok(raw) = read_marker(path) else {
                    continue;
                };
                let Ok(pid) = raw.parse::<i32>() else {
                    continue;
                };
                if pid > 1 {
                    // SAFETY: SIGKILL targets only a pid written by this fixture's shell.
                    let _ = unsafe { kill(pid, 9) };
                }
            }
        }
    }

    let dir = marion_testsupport::scratch("pty-unreaped-pgid-holder");
    let holder_file = dir.join("holder.pid");
    let leader_marker = dir.join("leader.started");
    let spawned_pid_file = dir.join("holder.spawned-pid");
    let entered_marker = dir.join("holder.entered");
    let trapped_marker = dir.join("holder.trapped");
    let written_marker = dir.join("holder.pid-written");
    let master = PtyMaster::open(WinSize::new(80, 24)).unwrap();
    let host = PtyHost::start(
        AgentId("unreaped-pgid-holder".into()),
        master,
        &dir.join("pty.cast"),
        WinSize::new(80, 24),
        "xterm-256color",
        Instant::now(),
    )
    .unwrap();
    let mut command = sh("printf 'started' >\"$LEADER_MARKER\"; \
         (printf 'entered' >\"$ENTERED_MARKER\"; trap '' HUP TERM; \
          printf 'trapped' >\"$TRAPPED_MARKER\"; \
          while [ ! -s \"$HOLDER_FILE\" ]; do :; done; \
          printf 'written' >\"$WRITTEN_MARKER\"; exec sleep 30) & \
         holder_pid=$!; printf '%s' \"$holder_pid\" >\"$SPAWNED_PID_FILE\"; \
         printf '%s' \"$holder_pid\" >\"$HOLDER_FILE\"; \
         while [ ! -s \"$TRAPPED_MARKER\" ] || [ ! -s \"$WRITTEN_MARKER\" ]; do :; done; \
         printf 'leader-tail'; exit 37");
    command
        .env("HOLDER_FILE", &holder_file)
        .env("LEADER_MARKER", &leader_marker)
        .env("SPAWNED_PID_FILE", &spawned_pid_file)
        .env("ENTERED_MARKER", &entered_marker)
        .env("TRAPPED_MARKER", &trapped_marker)
        .env("WRITTEN_MARKER", &written_marker);
    let child = spawn_pty(
        witness(),
        &mut command,
        host.master(),
        StdinPlan::TerminalSlave,
        None,
    )
    .unwrap();
    // Installed before adoption, so either published pid is still swept if anything below unwinds.
    let mut cleanup = KillCandidatesOnDrop::new(&holder_file, &spawned_pid_file);
    host.adopt(child);

    let holder_ready = until(|| {
        matches!(read_marker(&trapped_marker).as_deref(), Ok("trapped"))
            && matches!(read_marker(&written_marker).as_deref(), Ok("written"))
            && read_marker(&holder_file)
                .ok()
                .and_then(|pid| pid.parse::<i32>().ok())
                .is_some_and(|pid| pid > 1)
    });
    if !holder_ready {
        panic!(
            "the slave holder never became ready; dir={}; leader={:?}; spawned_pid={:?}; entered={:?}; trapped={:?}; holder_pid={:?}; pid_written={:?}",
            dir.display(),
            read_marker(&leader_marker),
            read_marker(&spawned_pid_file),
            read_marker(&entered_marker),
            read_marker(&trapped_marker),
            read_marker(&holder_file),
            read_marker(&written_marker),
        );
    }
    let holder_pid: i32 = read_marker(&holder_file).unwrap().parse().unwrap();
    assert!(
        until(|| host
            .poll_exited_unreaped()
            .expect("non-consuming host poll")),
        "the leader never exited"
    );
    assert!(
        alive(holder_pid),
        "the same-group slave holder is the fixture"
    );

    let status = host.shutdown().unwrap().expect("the leader is reaped once");
    assert_eq!(
        status.code(),
        Some(37),
        "the leader's exact status survives"
    );
    assert!(
        until(|| !alive(holder_pid)),
        "the same-group slave holder survived shutdown"
    );
    let retained = host.retention_snapshot();
    let output: Vec<u8> = retained
        .iter()
        .filter_map(|record| match &record.kind {
            super::splice::DisplayKind::Output(bytes) => Some(bytes.as_ref()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    assert_eq!(output, b"leader-tail");
    assert_eq!(
        retained
            .iter()
            .filter(|record| matches!(record.kind, super::splice::DisplayKind::End))
            .count(),
        1
    );
    assert!(matches!(
        retained.last().map(|r| &r.kind),
        Some(super::splice::DisplayKind::End)
    ));
    host.completed_replay_charge()
        .expect("real EOF after the group sweep is completion-eligible");
    cleanup.disarm();
}

/// The other half of point 6, and of point 8: once the child is gone and nobody holds a slave, the
/// master reports end of stream. On Linux that arrives as `EIO`, which [`PtyMaster::read`] maps to
/// `Ok(0)`; on macOS the kernel returns 0 directly.
///
/// **Mutation:** treat `EIO` as an error (red on Linux); keep the parent's slave copies (red
/// everywhere).
#[test]
fn the_master_reaches_eof_once_the_child_exits() {
    let dir = marion_testsupport::scratch("pty-eof");
    let master = PtyMaster::open(WinSize::new(80, 24)).unwrap();
    let host = PtyHost::start(
        AgentId("eof".into()),
        master,
        &dir.join("pty.cast"),
        WinSize::new(80, 24),
        "xterm-256color",
        Instant::now(),
    )
    .unwrap();
    let child = spawn_pty(
        witness(),
        &mut sh("printf 'done\\n'"),
        host.master(),
        StdinPlan::TerminalSlave,
        None,
    )
    .unwrap();
    let pid = child.pid();
    host.adopt(child);

    assert!(
        until(|| host.bytes_read() > 0),
        "the child's output never arrived (child {}alive)",
        if alive(pid) { "" } else { "not " }
    );
    // `shutdown` reaps and then joins the reader. If EOF were reported as a fault the join would
    // still return — the loop breaks on an error too — so the assertion that matters is that the
    // recording is intact and no read failure was logged as one.
    let status = host.shutdown().unwrap().expect("reaped");
    assert!(status.success() || status.code() == Some(0), "{status:?}");
    let (_, records) = read_cast(&dir.join("pty.cast"));
    let out: String = records
        .iter()
        .filter(|(_, c, _)| c == "o")
        .map(|(_, _, d)| d.as_str())
        .collect();
    assert!(out.contains("done"), "{out:?}");
    assert_eq!(
        records.last().map(|(_, c, _)| c.as_str()),
        Some("x"),
        "the exit record is last"
    );
}

/// **Closing the master hangs up the child**, which is the property the whole "the supervisor owns
/// the master" argument rests on: if a client held it, this is what a client crash would do to
/// every agent in a pane.
///
/// **Mutation:** remove `TIOCSCTTY` from `pre_exec` — the child then has no controlling terminal,
/// the hangup reaches nobody, and the `sleep` runs to completion.
///
/// The wait is bounded and the failure is an assertion. `kill(pid, 0)` is consulted first so the
/// message distinguishes "still running" from "died of something else".
#[test]
fn closing_the_master_hangs_up_the_child() {
    use std::os::unix::process::ExitStatusExt;

    let master = PtyMaster::open(WinSize::new(80, 24)).unwrap();
    // `sh` must be *executing* the sleep, not have exec'd away, for the signal to be observable as
    // the shell's own death; `exec sleep` makes the reported signal the sleep's, which is the same
    // assertion either way.
    let mut child = spawn_pty(
        witness(),
        &mut sh("exec sleep 30"),
        &master,
        StdinPlan::TerminalSlave,
        None,
    )
    .unwrap();
    let pid = child.pid();
    // Give the exec a moment: a SIGHUP delivered before `sleep` replaces `sh` would be reported as
    // the shell's death, which is still SIGHUP — but the interesting case is the steady state.
    assert!(until(|| alive(pid)), "the child never started");
    std::thread::sleep(Duration::from_millis(50));

    drop(master);

    let mut status = None;
    assert!(
        until(|| {
            status = child.try_wait().expect("try_wait");
            status.is_some()
        }),
        "the child outlived its terminal: pid {pid} is {}. Without TIOCSCTTY the pty is not its \
         controlling terminal and the kernel's hangup reaches nobody.",
        if alive(pid) { "alive" } else { "gone" }
    );
    assert_eq!(
        status.unwrap().signal(),
        Some(1),
        "SIGHUP, not some other death"
    );
}

// ---------------------------------------------------------------------------------------------
// the M2 regression guard
// ---------------------------------------------------------------------------------------------

/// **The supervisor holds the master; a client is a listener.**
///
/// M2's criterion is that a crashed client leaves every node exactly as it was (§7.3.1). A pty is
/// the one resource where that is not automatic: the master *is* the terminal, and whoever holds it
/// can kill the agent by dying. So a client's departure must be able to do nothing worse than
/// shorten a subscriber list.
///
/// The dead client is simulated by dropping the receiving end of its outbound queue, which is
/// exactly what a SIGKILLed client's socket does — `Outbound::send` sees `Disconnected` and reports
/// the connection finished.
///
/// **Mutation:** move the `PtyMaster` (or an `Arc` clone of it) into the listener, so dropping the
/// listener closes the fd. The child then dies of SIGHUP and both later assertions fail.
#[test]
fn the_supervisor_and_not_the_client_holds_the_master() {
    let dir = marion_testsupport::scratch("pty-m2");
    let master = PtyMaster::open(WinSize::new(80, 24)).unwrap();
    let host = PtyHost::start(
        AgentId("m2".into()),
        master,
        &dir.join("pty.cast"),
        WinSize::new(80, 24),
        "xterm-256color",
        Instant::now(),
    )
    .unwrap();
    let child = spawn_pty(
        witness(),
        // Keeps talking, so "fresh bytes after the first client died" is a real observation and not
        // a replay of something buffered before it.
        &mut sh("while :; do printf 'tick\\n'; sleep 0.05; done"),
        host.master(),
        StdinPlan::TerminalSlave,
        None,
    )
    .unwrap();
    let pid = child.pid();
    host.adopt(child);

    // Client A attaches and hears something.
    let (a, a_rx) = crate::serve::capture(crate::serve::ConnId(1));
    host.listen(a);
    assert!(
        until(|| a_rx.try_recv().is_ok()),
        "client A never received a node/pty frame"
    );

    // Client A is SIGKILLed: its queue's receiver goes away.
    let seq_at_death = host.next_seq();
    drop(a_rx);
    assert!(
        until(|| host.listeners() == 0),
        "the dead listener was never garbage-collected"
    );

    // (i) the node is untouched.
    assert!(
        alive(pid),
        "the child died when its client did — the master was in the wrong process"
    );

    // (ii) a second client receives *fresh* bytes, not a replay of a corpse.
    let (b, b_rx) = crate::serve::capture(crate::serve::ConnId(2));
    host.listen(b);
    let mut got = None;
    assert!(
        until(|| {
            match b_rx.try_recv() {
                Ok(frame) => {
                    got = Some(frame);
                    true
                }
                Err(_) => false,
            }
        }),
        "client B received nothing after client A died (child {}alive)",
        if alive(pid) { "" } else { "not " }
    );
    let frame: serde_json::Value =
        serde_json::from_slice(&got.unwrap()).expect("a JSON-RPC notification");
    assert_eq!(frame["method"], "node/pty");
    assert!(
        frame["params"]["seq"].as_u64().unwrap() >= seq_at_death,
        "B was handed an ordinal from before A died, so this is replay and not a live stream: \
         {frame}"
    );
    assert!(frame["params"]["bytes"].as_str().unwrap().contains("tick"));

    host.shutdown().unwrap();
    assert!(!alive(pid) || until(|| !alive(pid)), "the child was reaped");
}

// ---------------------------------------------------------------------------------------------
// pty.cast
// ---------------------------------------------------------------------------------------------

/// The shape, checked against the five committed captures rather than against memory.
///
/// The comparison is on *decoded* values, not bytes: `extract.py` used Python's `json.dumps`, which
/// escapes every non-ASCII character, while `serde_json` emits UTF-8 directly. Both are valid JSON
/// for the same string, and pinning the byte spelling would pin Python's defaults.
#[test]
fn the_cast_header_and_record_shape_match_the_committed_captures() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/s2/codex-cli-0.146.0-boot-status-help-resize.cast");
    let (want_header, want_records) = read_cast(&fixture);

    let dir = marion_testsupport::scratch("pty-cast-shape");
    let path = dir.join("pty.cast");
    let mut w = CastWriter::create(
        &path,
        WinSize::new(120, 40),
        "xterm-256color",
        Instant::now(),
    )
    .unwrap();
    w.output("\u{1b}[?1049h").unwrap();
    w.input("/help").unwrap();
    w.resize(WinSize::new(100, 24)).unwrap();
    w.exit("0").unwrap();
    drop(w);
    let (header, records) = read_cast(&path);

    let mut keys: Vec<_> = header.as_object().unwrap().keys().cloned().collect();
    let mut want_keys: Vec<_> = want_header.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    want_keys.sort();
    assert_eq!(keys, want_keys, "header key set");
    assert_eq!(header["version"], 3, "asciicast v3");
    assert_eq!(header["term"]["cols"], 120);
    assert_eq!(header["term"]["rows"], 40);
    assert_eq!(header["term"]["type"], "xterm-256color");
    assert_eq!(header["env"]["TERM"], "xterm-256color");

    assert_eq!(
        records
            .iter()
            .map(|(_, c, _)| c.as_str())
            .collect::<Vec<_>>(),
        ["o", "i", "r", "x"]
    );
    // **`"COLSxROWS"`, and the committed captures are what says so**: the 0.146.0 resize capture
    // carries `"100x24"` for the 100-column, 24-row step, so the columns come first.
    let want_resize = want_records
        .iter()
        .find(|(_, c, _)| c == "r")
        .expect("the fixture resizes");
    assert_eq!(want_resize.2, "100x24", "the fixture pins the order");
    assert_eq!(records[2].2, "100x24", "and marion writes the same");
    // Intervals are relative and non-negative, exactly as `extract.py` produced them.
    assert!(records.iter().all(|(dt, _, _)| *dt >= 0.0));
    assert!(want_records.iter().all(|(dt, _, _)| *dt >= 0.0));
}

/// **`i` and `r` are not optional.** C1's mouse-through leaves no trace in the output stream at
/// all, and C2's `CSI 3J` finding cannot be re-derived after the fact without knowing the geometry
/// each repaint happened at.
///
/// **Mutations:** drop the `i` record from `PtyHost::write_input`; write the size as
/// `"ROWSxCOLS"`.
#[test]
fn the_cast_records_i_and_r_not_only_o() {
    let mut lb = Loopback::new("pty-ir", WinSize::new(120, 40));
    let lease = lb
        .host
        .lease_writer(crate::serve::ConnId(1))
        .expect("a fresh host has no writer");
    lb.host.write_input(&lease, b"/help\r").unwrap();
    lb.host.resize(WinSize::new(100, 24)).unwrap();
    lb.child_writes(b"repainted");
    lb.hang_up();
    lb.host.shutdown().unwrap();

    let (_, records) = read_cast(&lb.cast);
    let codes: Vec<&str> = records.iter().map(|(_, c, _)| c.as_str()).collect();
    assert!(codes.contains(&"i"), "no keystroke record: {codes:?}");
    assert!(codes.contains(&"r"), "no resize record: {codes:?}");
    assert!(codes.contains(&"o"), "no output record: {codes:?}");

    let i = records.iter().find(|(_, c, _)| c == "i").unwrap();
    assert_eq!(i.2, "/help\r");
    let r = records.iter().find(|(_, c, _)| c == "r").unwrap();
    assert_eq!(
        r.2, "100x24",
        "columns first — `\"24x100\"` is the transposition the committed captures rule out"
    );
    assert_eq!(
        lb.host.master().size().unwrap(),
        WinSize::new(100, 24),
        "and the kernel really was resized, so the record is not a lie"
    );

    let stream = read_stream(&lb.cast);
    assert!(stream.records.iter().any(|record| matches!(
        record.kind,
        super::stream::RecordKind::InputEvidence { byte_len: 6, .. }
    )));
}

#[test]
fn resolved_input_delivery_keeps_completed_replay_eligible() {
    let mut lb = Loopback::new("pty-input-delivery-completed", WinSize::new(80, 24));
    let lease = lb.host.lease_writer(ConnId(85)).unwrap();

    lb.host
        .write_input(&lease, b"delivered\r")
        .expect("the master accepts the admitted input");
    lb.hang_up();
    lb.host.shutdown().expect("legacy shutdown succeeds");

    assert!(
        lb.host.completed_replay_charge().is_ok(),
        "a resolved successful delivery must not poison completion"
    );
    let (_, records) = read_cast(&lb.cast);
    let codes = records
        .iter()
        .map(|(_, code, _)| code.as_str())
        .collect::<Vec<_>>();
    assert!(codes.contains(&"i"), "{codes:?}");
    assert_eq!(codes.last(), Some(&"x"), "{codes:?}");
}

/// **Every `o` record replays at the geometry the node actually emitted it at.**
///
/// This is the property `r` records exist for, and it is strictly stronger than the one this test
/// used to assert. The old version forced an interleaving and then checked only `r < o` — so it
/// passed while the labelling was wrong, which is the worst state a test can be in: the defect it
/// was written for was live underneath it, and its green was the reason nobody looked.
///
/// What was wrong: `PtyHost::resize` wrote the `r`, **fsynced it**, released the cast lock, and
/// only then issued `TIOCSWINSZ`. For the whole of that window the terminal was still `120x40`, so
/// anything the child wrote in it was recorded after a record claiming `100x24`. Replay then
/// applies the new geometry to bytes painted at the old one and every absolute cursor address after
/// it lands in the wrong place.
///
/// # How the geometry becomes observable
///
/// The marker carries what the **slave** says about itself. The hook asks `TIOCGWINSZ` on the slave
/// fd and writes `MARK<COLSxROWS>` with the answer, so each `o` record states the size the node was
/// at when it produced it, and the assertion is a comparison rather than an inference. Three
/// markers, at the three positions that can exist: before the resize is asked for, inside the
/// window while it is in flight, and after `resize` has returned.
///
/// The check is then total — *every* marker against the last `r` before it, with the header's size
/// standing in when there is none — so it does not depend on which side of the boundary the middle
/// marker lands on. Whichever it is, it has to be labelled consistently, and that is what makes
/// this deterministic rather than a race the test has to win.
///
/// **Mutations, each of which this kills and the old test did not:**
/// * restore caller-side `record` → `fsync` → `TIOCSWINSZ` (the original defect): the middle marker
///   reads `120x40` and lands after the `100x24` record;
/// * swap `apply_pending_resize` to record-then-ioctl without the cast lock held across both: the
///   child's `SIGWINCH` repaint is recorded ahead of the record explaining it;
/// * drop the `r` record entirely: the markers after the resize have no matching record at all.
#[test]
fn every_output_record_replays_at_the_geometry_it_was_emitted_at() {
    const BEFORE: WinSize = WinSize {
        cols: 120,
        rows: 40,
    };
    const AFTER: WinSize = WinSize {
        cols: 100,
        rows: 24,
    };

    let mut lb = Loopback::new("pty-resize-geometry", BEFORE);
    let slave = lb.slave.take().expect("attached");
    let slave_fd = slave.as_raw_fd();
    let counter = lb.host.bytes_counter();
    let slave = Arc::new(Mutex::new(slave));

    // Write a marker stamped with the size the slave itself reports. **No waiting inside it**: the
    // hook below runs *on the reader thread*, so a wait for the recorder there would be a wait for
    // the thread doing the waiting. Ordering comes from where each call site sits instead.
    let mark = {
        let slave = Arc::clone(&slave);
        move || {
            let size = slave_size(slave_fd);
            let text = format!("MARK<{}x{}>", size.cols, size.rows);
            slave
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .write_all(text.as_bytes())
                .expect("the slave accepts a write");
            text.len() as u64
        }
    };
    // On the test's own thread the wait is both possible and needed, so the marker is in the file
    // before the next step rather than in the pty buffer.
    let mark_and_wait = |mark: &dyn Fn() -> u64| {
        let before = counter.load(Ordering::SeqCst);
        let n = mark();
        assert!(
            until(|| counter.load(Ordering::SeqCst) >= before + n),
            "the marker never reached the recorder"
        );
    };

    mark_and_wait(&mark);

    // The hook runs inside `apply_pending_resize`'s drain, before the `TIOCSWINSZ`: the node speaks
    // at the *old* geometry with a resize to a new one already in flight. That is the window the
    // old caller-side order left open between its `r` record and its ioctl, so a mutation that
    // reintroduces it lands here.
    {
        let mark = mark.clone();
        lb.host.set_resize_hook(Box::new(move || {
            mark();
        }));
    }
    lb.host.resize(AFTER).unwrap();
    lb.host.clear_resize_hook();

    assert_eq!(
        lb.host.master().size().unwrap(),
        AFTER,
        "the kernel really was resized, so the record is not a lie"
    );
    mark_and_wait(&mark);

    drop(slave);
    lb.hang_up();
    lb.host.shutdown().unwrap();

    // Replay: walk the records in order, tracking the geometry a replayer would be at, and require
    // every marker to agree with it.
    let (header, records) = read_cast(&lb.cast);
    let mut at = format!(
        "{}x{}",
        header["term"]["cols"].as_u64().expect("a header width"),
        header["term"]["rows"].as_u64().expect("a header height")
    );
    let mut checked = 0;
    for (i, (_, code, data)) in records.iter().enumerate() {
        match code.as_str() {
            "r" => at = data.clone(),
            "o" => {
                for observed in markers(data) {
                    checked += 1;
                    assert_eq!(
                        observed, at,
                        "record {i} was emitted at {observed} but replays at {at} — output painted \
                         at one geometry and labelled with another is exactly what an `r` record \
                         exists to prevent.\n{records:#?}"
                    );
                }
            }
            _ => {}
        }
    }
    assert_eq!(
        checked, 3,
        "all three markers must be in the cast, or the walk above proved nothing: {records:#?}"
    );
    assert!(
        records.iter().any(|(_, c, d)| c == "r" && d == "100x24"),
        "the resize itself must be recorded: {records:#?}"
    );
}

/// Every `MARK<COLSxROWS>` in one `o` record's payload, as `"COLSxROWS"`.
///
/// A record can carry more than one — the recorder writes whatever a single `read` returned, and
/// two markers can share a chunk — so this yields all of them rather than the first.
fn markers(data: &str) -> Vec<String> {
    data.split("MARK<")
        .skip(1)
        .filter_map(|rest| rest.split_once('>').map(|(size, _)| size.to_string()))
        .collect()
}

/// What the **slave** says its own geometry is, which is what the node sees.
///
/// Asked of the slave rather than of `PtyMaster::size` deliberately: the two are the same object to
/// the kernel, but the claim under test is about what the *node* was painting at, and reading it
/// through the master would be marion checking its own bookkeeping.
fn slave_size(fd: RawFd) -> WinSize {
    let mut ws = RawWinSize::default();
    // SAFETY: `fd` is an open pts and `&mut ws` is a live, correctly-shaped `struct winsize`.
    let rc = unsafe { ioctl(fd, sys::TIOCGWINSZ, &raw mut ws) };
    assert_eq!(
        rc,
        0,
        "TIOCGWINSZ on the slave: {}",
        io::Error::last_os_error()
    );
    WinSize::new(ws.col, ws.row)
}

/// **One epoch for `events.jsonl` and `pty.cast`.**
///
/// §4.2 gives `mono_ns` the job of aligning the two files, and alignment against two different
/// zeroes is not alignment. `EventWriter.start` is set per node at `open`; the cast borrows it.
///
/// **Mutation:** have `CastWriter::create` call `Instant::now()` itself. The first interval then
/// measures from the cast's own birth, so it collapses to ~0 and the assertion below fails.
#[test]
fn mono_ns_and_the_cast_share_one_epoch() {
    use crate::events::EventWriter;
    use marion_core::event::Payload;

    let dir = marion_testsupport::scratch("pty-epoch");
    let mut w =
        EventWriter::open_path(&dir.join("events.jsonl"), &AgentId("epoch".into())).unwrap();

    // Stand in for the node's setup: the directory, the journal write, the ready gate. Everything
    // that happens between opening the event stream and opening the pty.
    let setup = Duration::from_millis(60);
    std::thread::sleep(setup);

    let mut cast = CastWriter::create(
        &dir.join("pty.cast"),
        WinSize::new(80, 24),
        "xterm",
        w.origin(),
    )
    .unwrap();
    cast.output("first").unwrap();
    let ev = w
        .append(crate::events::Draft::marion(Payload::Raw("x".into())))
        .unwrap();

    let (_, records) = read_cast(&dir.join("pty.cast"));
    let first_interval = records[0].0;
    let event_secs = ev.mono_ns as f64 / 1e9;
    let floor = setup.as_secs_f64();

    assert!(
        first_interval >= floor,
        "the cast's first record is {first_interval:.4}s after its origin, which is less than the \
         {floor:.4}s of setup that preceded it — so the cast minted its own epoch and `mono_ns` \
         anchors nothing"
    );
    assert!(
        (first_interval - event_secs).abs() < 0.05,
        "the two files disagree about now by {:.4}s (cast {first_interval:.4}, events \
         {event_secs:.4})",
        (first_interval - event_secs).abs()
    );
}

// ---------------------------------------------------------------------------------------------
// a read() is not a character
// ---------------------------------------------------------------------------------------------

/// **The reader carries an incomplete UTF-8 sequence across reads.**
///
/// This is not a hypothetical: `tests/fixtures/s2/NOTES.txt` records 23 U+FFFD in 9 regions across
/// three committed `.cast` files, caused by `extract.py` calling `decode(..., "replace")` per pty
/// read chunk. The captures themselves are valid UTF-8 end to end. A `from_utf8_lossy` per read
/// here would reproduce the same damage in production.
///
/// **Mutation:** replace `Utf8Stream::push` with `String::from_utf8_lossy(chunk).into_owned()`.
#[test]
fn the_reader_carries_an_incomplete_utf8_sequence_across_reads() {
    // U+256D, the box-drawing glyph Claude Code's banner is built from: E2 95 AD.
    let glyph = "╭".as_bytes();
    assert_eq!(glyph.len(), 3);

    let mut s = Utf8Stream::default();
    let mut out = String::new();
    for b in glyph {
        out.push_str(&s.push(std::slice::from_ref(b)));
    }
    assert_eq!(out, "╭", "one glyph, split three ways, arrives whole");
    assert!(!out.contains('\u{FFFD}'));
    assert_eq!(s.pending(), 0);

    // The same claim over a real capture, at every chunk size that could split something.
    let raw = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/s2/claude-2.1.220-boot-exit.raw.bin"),
    )
    .unwrap();
    let whole = std::str::from_utf8(&raw).expect("the capture is valid UTF-8 end to end");
    for chunk in [1usize, 2, 3, 7, 64, 1024] {
        let mut s = Utf8Stream::default();
        let mut got = String::new();
        for part in raw.chunks(chunk) {
            got.push_str(&s.push(part));
        }
        got.push_str(&s.finish());
        assert_eq!(got, whole, "chunked at {chunk} B");
        assert_eq!(
            got.matches('\u{FFFD}').count(),
            0,
            "chunked at {chunk} B produced replacement characters"
        );
    }

    // The counter-demonstration: per-chunk lossy decoding is what damaged the committed `.cast`
    // files, and it still does.
    let lossy: String = raw
        .chunks(7)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect();
    assert!(
        lossy.matches('\u{FFFD}').count() > 0,
        "if this stops being true the test above has stopped proving anything"
    );

    // **End to end, through the reader thread.** The unit above pins `Utf8Stream`; this pins that
    // `read_loop` uses it. Each byte is written separately and waited for, so the host really does
    // see three reads and not one.
    let mut lb = Loopback::new("pty-utf8", WinSize::new(80, 24));
    for b in "╭─╮".as_bytes() {
        lb.child_writes(std::slice::from_ref(b));
    }
    lb.hang_up();
    lb.host.shutdown().unwrap();
    let (_, records) = read_cast(&lb.cast);
    let seen: String = records
        .iter()
        .filter(|(_, c, _)| c == "o")
        .map(|(_, _, d)| d.as_str())
        .collect();
    assert_eq!(
        seen, "╭─╮",
        "the recorder split a glyph across reads — exactly the `extract.py` defect NOTES.txt \
         records, reproduced in production"
    );

    // The carry is bounded: a truncated sequence is at most three bytes.
    let mut s = Utf8Stream::default();
    s.push(&raw[..raw.len() - 1]);
    assert!(s.pending() <= 3);
    // And an invalid byte is replaced once, in place, rather than stalling the stream.
    let mut s = Utf8Stream::default();
    assert_eq!(s.push(b"a\xffb"), "a\u{FFFD}b");
    assert_eq!(s.pending(), 0);
}

/// Retention is fed from the raw pty read, before the lossy UTF-8 view used by the cast and live
/// listener paths. A replacement character in either retained position would make replay
/// byte-inexact for terminal control streams.
#[test]
fn pty_retention_keeps_invalid_utf8_output_byte_exact() {
    let mut lb = Loopback::new("pty-retention-raw", WinSize::new(80, 24));
    let raw: &[u8] = b"before\xffafter";
    lb.child_writes(raw);

    let retained = lb.host.retention_snapshot();
    assert_eq!(retained.len(), 1, "one pty read must be retained once");
    let super::splice::DisplayKind::Output(bytes) = &retained[0].kind else {
        panic!("first retained record must be Output");
    };
    assert_eq!(bytes.as_ref(), raw);

    lb.hang_up();
    lb.host.shutdown().unwrap();
}

#[test]
fn pty_retention_orders_resize_between_raw_outputs() {
    let mut lb = Loopback::new("pty-retention-resize", WinSize::new(80, 24));
    lb.child_writes(b"before");
    lb.host.resize(WinSize::new(100, 40)).unwrap();
    lb.child_writes(b"after");

    let retained = lb.host.retention_snapshot();
    assert_eq!(
        retained.iter().map(|record| record.seq).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(matches!(
        &retained[0].kind,
        super::splice::DisplayKind::Output(bytes) if bytes.as_ref() == b"before"
    ));
    assert!(matches!(
        retained[1].kind,
        super::splice::DisplayKind::Resize {
            rows: 40,
            cols: 100
        }
    ));
    assert!(matches!(
        &retained[2].kind,
        super::splice::DisplayKind::Output(bytes) if bytes.as_ref() == b"after"
    ));

    lb.hang_up();
    lb.host.shutdown().unwrap();
}

#[test]
fn pty_retention_ends_once_and_accepts_no_later_record() {
    let mut lb = Loopback::new("pty-retention-end", WinSize::new(80, 24));
    lb.child_writes(b"last");
    lb.hang_up();
    lb.host.shutdown().unwrap();

    let ended = lb.host.retention_snapshot();
    assert_eq!(
        ended.iter().map(|record| record.seq).collect::<Vec<_>>(),
        [1, 2]
    );
    assert!(matches!(
        &ended[0].kind,
        super::splice::DisplayKind::Output(bytes) if bytes.as_ref() == b"last"
    ));
    assert!(matches!(ended[1].kind, super::splice::DisplayKind::End));

    lb.host.resize(WinSize::new(100, 40)).unwrap();
    lb.host.shutdown().unwrap();
    assert_eq!(lb.host.retention_snapshot(), ended);
}

/// A reader panic makes replay ineligible without changing the legacy node outcome. The production
/// change that must make this fail is either publishing from `join` alone or returning before the
/// cast's exit record: one lies about replay, the other changes production-dark core behavior.
#[test]
fn reader_panic_keeps_core_shutdown_but_refuses_completed_replay() {
    let mut lb = Loopback::new("pty-reader-panic-no-end", WinSize::new(80, 24));
    *lb.host
        .shared
        .pane_splice_outcome_hook
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(Box::new(|| panic!("controlled reader panic")));
    lb.slave
        .as_mut()
        .expect("still attached")
        .write_all(b"partial")
        .expect("the slave accepts the panic trigger");
    assert!(
        until(|| !lb
            .host
            .shared
            .resize
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reader),
        "the controlled reader did not unwind"
    );
    lb.hang_up();

    lb.host
        .shutdown()
        .expect("dark replay cannot change legacy shutdown success");
    let error = lb
        .host
        .completed_replay_charge()
        .expect_err("a panicked reader without End is not a completed pane");
    assert!(error.to_string().contains("reader"), "{error}");
    let (_, cast) = read_cast(&lb.cast);
    assert!(cast.iter().any(|(_, code, _)| code == "x"));
    assert!(
        !lb.host
            .retention_snapshot()
            .iter()
            .any(|record| matches!(record.kind, super::splice::DisplayKind::End)),
        "the panic fixture must genuinely lack End"
    );
}

/// Setting the host's stop flag is not evidence that the terminal stream ended. This fixture keeps
/// a real slave fd open while shutdown asks the non-blocking reader to leave, so the reader can
/// only observe `WouldBlock`, never kernel EOF. The legacy shutdown and cast exit still complete,
/// but a replay without a terminal EOF must not grow a synthetic End or become cacheable.
#[test]
fn stopped_reader_without_kernel_eof_never_forges_end() {
    let lb = Loopback::new("pty-stopped-without-eof", WinSize::new(80, 24));
    assert!(lb.slave.is_some(), "the kernel slave must remain open");

    lb.host
        .shutdown()
        .expect("forced reader stop stays dark to legacy shutdown");

    assert!(
        !lb.host
            .retention_snapshot()
            .iter()
            .any(|record| matches!(record.kind, super::splice::DisplayKind::End)),
        "stopped plus WouldBlock forged a terminal End without kernel EOF"
    );
    let error = lb
        .host
        .completed_replay_charge()
        .expect_err("a reader stopped before kernel EOF is not a completed pane");
    assert!(error.to_string().contains("reader"), "{error}");
    let (_, cast) = read_cast(&lb.cast);
    assert_eq!(
        cast.last().map(|(_, code, _)| code.as_str()),
        Some("x"),
        "legacy cast exit must still be the final record"
    );
}

/// Shutdown gives the kernel a bounded opportunity to report the real EOF produced by teardown.
/// The reader is parked on `WouldBlock` until shutdown has completed its process sweep; closing
/// the last slave then must yield the real terminal End instead of losing it to an eager stop flag.
#[test]
fn shutdown_grace_retains_real_eof_that_arrives_after_process_sweep() {
    let dir = marion_testsupport::scratch("pty-shutdown-eof-grace");
    let master = PtyMaster::open(WinSize::new(80, 24)).unwrap();
    let slave = std::fs::File::from(master.open_slave().unwrap());
    let host = Arc::new(
        PtyHost::start(
            AgentId("shutdown-eof-grace".into()),
            master,
            &dir.join("pty.cast"),
            WinSize::new(80, 24),
            "xterm-256color",
            Instant::now(),
        )
        .unwrap(),
    );
    let (reader_tx, reader_rx) = std::sync::mpsc::sync_channel(1);
    let (reader_release_tx, reader_release_rx) = std::sync::mpsc::sync_channel(1);
    let reader_release_rx = Mutex::new(reader_release_rx);
    host.set_reader_would_block_hook(Box::new(move || {
        reader_tx.send(()).expect("the assertion side is alive");
        reader_release_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(Duration::from_secs(2))
            .expect("the reader hook was not released");
    }));
    reader_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the reader never reached WouldBlock");

    let (before_stop_tx, before_stop_rx) = std::sync::mpsc::sync_channel(1);
    let (allow_stop_tx, allow_stop_rx) = std::sync::mpsc::sync_channel(1);
    let allow_stop_rx = Mutex::new(allow_stop_rx);
    host.set_before_reader_stop_hook(Box::new(move || {
        before_stop_tx
            .send(())
            .expect("the assertion side is alive");
        allow_stop_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(Duration::from_secs(2))
            .expect("the shutdown stop boundary was not released");
    }));
    let (drain_tx, drain_rx) = std::sync::mpsc::sync_channel(1);
    host.set_reader_drain_hook(Box::new(move || {
        drain_tx.send(()).expect("the assertion side is alive");
    }));
    let shutdown_host = Arc::clone(&host);
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let shutdown = std::thread::spawn(move || {
        done_tx
            .send(shutdown_host.shutdown())
            .expect("the assertion side is alive");
    });
    before_stop_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("shutdown never reached its post-sweep stop boundary");

    drop(slave);
    allow_stop_tx
        .send(())
        .expect("the shutdown thread is alive");
    drain_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("shutdown never entered its bounded real-EOF drain");
    assert!(
        !host.stopped_for_test(),
        "shutdown stopped the reader before giving real EOF its drain grace"
    );
    reader_release_tx
        .send(())
        .expect("the reader thread is alive");
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("shutdown did not finish within its EOF grace")
        .expect("legacy shutdown succeeds");
    shutdown.join().unwrap();

    assert!(
        host.completed_replay_charge().is_ok(),
        "real EOF within the drain grace must qualify completion"
    );
    assert_eq!(
        host.retention_snapshot()
            .iter()
            .filter(|record| matches!(record.kind, super::splice::DisplayKind::End))
            .count(),
        1,
        "real EOF appends exactly one End"
    );
}

/// A retention cap is dark to the legacy node but disqualifies the completed replay. The production
/// change that must make this fail is propagating the latch through core shutdown, or ignoring it
/// and publishing a permanently truncated, non-terminal prefix.
#[test]
fn retention_latch_keeps_core_shutdown_but_refuses_completed_replay() {
    let mut lb = Loopback::with_splice_limits(
        "pty-retention-latch-no-end",
        WinSize::new(80, 24),
        super::splice::SpliceLimits::testing(3, 16),
    );
    lb.child_writes(b"too-large");
    lb.hang_up();

    lb.host
        .shutdown()
        .expect("dark replay cannot change legacy shutdown success");
    let error = lb
        .host
        .completed_replay_charge()
        .expect_err("a latched retention failure cannot become Completed");
    assert!(error.to_string().contains("retention"), "{error}");
    let (_, cast) = read_cast(&lb.cast);
    assert!(cast.iter().any(|(_, code, _)| code == "x"));
    assert!(lb.host.retention_error().is_some());
    assert!(
        !lb.host
            .retention_snapshot()
            .iter()
            .any(|record| matches!(record.kind, super::splice::DisplayKind::End)),
        "disabled retention must not forge End"
    );
}

/// `PtySplice` reserves its own subscriber records, but every late internal replay also grows the
/// host's `PaneStreams::slots` map. The Completed charge must reserve those outer slots at the
/// configured maximum before any late client arrives.
#[test]
fn completed_host_charge_reserves_future_pane_stream_slots() {
    fn outer_reserve(subscribers: usize) -> usize {
        let mut lb = Loopback::with_splice_limits(
            "pty-completion-pane-slot-reserve",
            WinSize::new(80, 24),
            super::splice::SpliceLimits::testing_with_subscribers(0, 2, subscribers, 0),
        );
        lb.hang_up();
        lb.host.shutdown().unwrap();
        let splice = lb
            .host
            .shared
            .splice
            .completion_charge()
            .expect("the charge fits")
            .expect("kernel EOF retained End");
        lb.host.completed_replay_charge().unwrap() - splice
    }

    let no_slots = outer_reserve(0);
    let four_slots = outer_reserve(4);
    assert!(
        no_slots >= COMPLETED_HOST_FIXED_RESERVE_BYTES,
        "the charge omits fixed cache-owned host/registry allocations: {no_slots}"
    );
    assert_eq!(
        four_slots - no_slots,
        4 * (PANE_STREAM_SLOT_RESERVE_BYTES + PANE_CANCELLATION_ARC_RESERVE_BYTES),
        "each future pane slot needs its map entry and separately allocated cancellation Arc"
    );
    assert!(
        completed_host_charge_from(usize::MAX, 0).is_none(),
        "fixed host accounting overflow must refuse completion, not wrap or panic"
    );
    assert!(
        completed_host_charge_from(0, usize::MAX).is_none(),
        "future cancellation/slot reserve overflow must refuse completion"
    );
}

/// A subscriber must be reachable by an emitter for the whole replay-to-ready handoff. The
/// production change that must make this fail is removing the pending registry entry before
/// installing the ready entry: an `End` retained in that gap supplies the only wake the queued
/// terminal record will ever receive.
#[test]
fn pane_replay_delivers_end_retained_during_the_ready_handoff() {
    let lb = Loopback::without_reader("pty-pane-ready-handoff", WinSize::new(80, 24));
    lb.host.shared.retain_output(b"prefix");
    let (out, rx) = crate::serve::capture(crate::serve::ConnId(101));
    let descriptor = lb
        .host
        .begin_pane_replay(out.conn(), out)
        .expect("the replay is reserved");

    let (hook, reached, release) = bounded_hook_gate();
    *lb.host
        .shared
        .pane_transition_hook
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(hook);

    lb.host
        .pane_ready(crate::serve::ConnId(101), &descriptor.token, descriptor.cut);
    let captured = std::thread::scope(|scope| {
        let driver = scope.spawn(move || rx.try_iter().collect::<Vec<_>>());
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("the replay reached its transition seam");
        lb.host.shared.retain_end();
        release.send(()).expect("the replay driver is alive");
        driver.join().unwrap()
    });

    let frames = captured
        .into_iter()
        .map(|bytes| {
            marion_core::proto::Frame::from_line(std::str::from_utf8(&bytes).unwrap()).unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        frames.len(),
        3,
        "the handoff dropped the end wake: {frames:?}"
    );
    assert!(matches!(
        &frames[0],
        marion_core::proto::Frame::Notification(note)
            if matches!(&note.event, Event::NodePaneFrame(frame)
                if frame.seq == 0
                    && matches!(frame.frame, PaneFrameKindV1::Resize { cols: 80, rows: 24 }))
    ));
    assert!(matches!(
        &frames[1],
        marion_core::proto::Frame::Notification(note)
            if matches!(&note.event, Event::NodePaneFrame(frame)
                if matches!(&frame.frame, PaneFrameKindV1::Output { bytes }
                    if bytes.as_bytes() == b"prefix"))
    ));
    assert!(matches!(
        &frames[2],
        marion_core::proto::Frame::Notification(note)
            if matches!(&note.event, Event::NodePaneFrame(frame)
                if matches!(frame.frame, PaneFrameKindV1::End {}))
    ));
}

/// Pane delivery is an outbound callback and therefore cannot run while the cast serialization
/// lock is held. The production change that must make this fail is retaining and dispatching a
/// resize from inside the cast critical section: the callback then observes that lock as busy.
#[test]
fn pane_resize_delivery_runs_after_the_cast_lock_is_released() {
    let lb = Loopback::without_reader("pty-pane-resize-lock", WinSize::new(80, 24));
    let (out, rx) = crate::serve::capture(crate::serve::ConnId(102));
    let descriptor = lb
        .host
        .begin_pane_replay(out.conn(), out)
        .expect("the replay is reserved");
    lb.host
        .pane_ready(crate::serve::ConnId(102), &descriptor.token, descriptor.cut);
    let initial = rx.try_recv().expect("initial geometry is replayed");
    assert!(matches!(
        Frame::from_line(std::str::from_utf8(&initial).unwrap()).unwrap(),
        Frame::Notification(note)
            if matches!(note.event, Event::NodePaneFrame(ref frame)
                if frame.seq == 0
                    && matches!(frame.frame, PaneFrameKindV1::Resize { cols: 80, rows: 24 }))
    ));
    assert!(
        rx.try_iter().next().is_none(),
        "initial replay reaches Ready"
    );

    let (observed_tx, observed_rx) = std::sync::mpsc::channel();
    let shared = Arc::downgrade(&lb.host.shared);
    *lb.host
        .shared
        .pane_delivery_hook
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(Box::new(move || {
        let shared = shared.upgrade().expect("the host still owns shared state");
        observed_tx
            .send(shared.recorders.try_lock().is_ok())
            .expect("the assertion side is alive");
    }));

    lb.host.resize(WinSize::new(100, 30)).unwrap();
    let _resize_frame = rx.try_recv().expect("the writer pulls the resize frame");
    assert!(
        observed_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the resize delivery hook did not run"),
        "the resize callback ran under the cast lock"
    );
}

/// A failed replacement is transactional: it cannot cancel the pane stream the connection
/// already owns. The production change that must make this fail is cancelling the existing slot
/// before noticing that it is `Transitioning`; the original replay then never becomes live.
#[test]
fn failed_replay_replacement_preserves_the_existing_subscription() {
    let lb = Loopback::without_reader("pty-pane-transaction", WinSize::new(80, 24));
    lb.host.shared.retain_output(b"prefix");
    let conn = crate::serve::ConnId(105);
    let (out, rx) = crate::serve::capture(conn);
    let descriptor = lb
        .host
        .begin_pane_replay(conn, out.clone())
        .expect("the original replay is reserved");

    let (hook, reached, release) = bounded_hook_gate();
    *lb.host
        .shared
        .pane_transition_hook
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(hook);

    lb.host.pane_ready(conn, &descriptor.token, descriptor.cut);
    let (rx, mut frames, replacement_rejected) = std::thread::scope(|scope| {
        let driver = scope.spawn(move || {
            let frames = rx.try_iter().collect::<Vec<_>>();
            (rx, frames)
        });
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("the replay reached its transition seam");
        let replacement_rejected = lb.host.begin_pane_replay(conn, out.clone()).is_none();
        release.send(()).expect("the replay driver is alive");
        let (rx, frames) = driver.join().unwrap();
        (rx, frames, replacement_rejected)
    });
    *lb.host
        .shared
        .pane_transition_hook
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    lb.host.shared.retain_output(b"live");

    frames.extend(rx.try_iter());
    assert!(
        replacement_rejected,
        "a transitioning slot accepts replacement"
    );
    assert_eq!(
        frames.len(),
        3,
        "the failed replacement cancelled the old live stream"
    );
}

/// Replaying a retained history larger than the connection queue must be paced by the writer, not
/// mistaken for a slow client. The production change that must make this fail is synchronously
/// calling `try_send` for every prefix record from the Ready notification handler.
#[test]
fn retained_replay_larger_than_the_outbound_queue_does_not_depart_a_healthy_client() {
    let mut lb = Loopback::with_splice_limits(
        "pty-pane-paced-prefix",
        WinSize::new(80, 24),
        super::splice::SpliceLimits::testing_with_subscribers(2_048, 1_100, 1, 1_100),
    );
    for _ in 0..1_025 {
        lb.host.shared.retain_output(b"x");
    }
    let conn = crate::serve::ConnId(106);
    let (out, rx) = crate::serve::capture(conn);
    let descriptor = lb
        .host
        .begin_pane_replay(conn, out.clone())
        .expect("the replay is reserved");

    lb.host.pane_ready(conn, &descriptor.token, descriptor.cut);

    let first = rx
        .try_recv()
        .expect("the writer pulls the first replay frame");
    assert!(out.notify(Event::NodePty {
        agent_id: AgentId("sentinel".into()),
        seq: 0,
        mono_ns: 0,
        bytes: "ordinary".into(),
    }));
    let frames = std::iter::once(first)
        .chain(rx.try_iter())
        .map(|bytes| Frame::from_line(std::str::from_utf8(&bytes).unwrap()).unwrap())
        .collect::<Vec<_>>();
    let mut pane = Vec::new();
    let mut sentinel_at = None;
    for (at, frame) in frames.iter().enumerate() {
        let Frame::Notification(note) = frame else {
            panic!("captured outbound item is not a notification: {frame:?}")
        };
        match &note.event {
            Event::NodePaneFrame(frame) => pane.push(frame),
            Event::NodePty {
                agent_id, bytes, ..
            } if agent_id.0 == "sentinel" && bytes == "ordinary" => sentinel_at = Some(at),
            other => panic!("unexpected captured event: {other:?}"),
        }
    }

    assert_eq!(
        out.departed(),
        None,
        "a healthy client was classified TooSlow solely because replay burst faster than its writer"
    );
    assert_eq!(
        pane.len(),
        1_026,
        "the writer must drain the complete prefix"
    );
    assert_eq!(
        pane.iter().map(|frame| frame.seq).collect::<Vec<_>>(),
        (0..1_026).collect::<Vec<_>>(),
        "replay remains dense and ordered"
    );
    assert!(matches!(
        &pane[0].frame,
        PaneFrameKindV1::Resize { cols: 80, rows: 24 }
    ));
    assert!(pane[1..].iter().all(|frame| {
        matches!(&frame.frame, PaneFrameKindV1::Output { bytes } if bytes.as_bytes() == b"x")
    }));
    assert!(
        sentinel_at.is_some_and(|at| at < frames.len() - 1),
        "an ordinary queued frame must run before replay completion: {sentinel_at:?} of {}",
        frames.len()
    );
    lb.hang_up();
}

/// A successfully written terminal `End` completes the subscriber; it cannot leave a permanent
/// Ready slot on an append-only stream that will never wake again. The production change that must
/// make this fail is installing Ready after End, so 64 open clients exhaust the subscriber cap and
/// the 65th completed replay is refused.
#[test]
fn completed_replay_releases_its_subscriber_while_connections_stay_open() {
    let lb = Loopback::without_reader("pty-pane-completed-release", WinSize::new(80, 24));
    lb.host.shared.retain_output(b"complete");
    lb.host.shared.retain_end();
    let mut open_connections = Vec::new();

    for ordinal in 0..65 {
        let conn = crate::serve::ConnId(1_000 + ordinal);
        let (out, captured) = crate::serve::capture(conn);
        let descriptor = lb
            .host
            .begin_pane_replay(conn, out.clone())
            .unwrap_or_else(|| panic!("completed replay {ordinal} exhausted a stale subscriber"));
        lb.host.pane_ready(conn, &descriptor.token, descriptor.cut);
        let frames = captured
            .try_iter()
            .map(|bytes| Frame::from_line(std::str::from_utf8(&bytes).unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(
            frames.last(),
            Some(Frame::Notification(note))
                if matches!(&note.event, Event::NodePaneFrame(frame)
                    if matches!(frame.frame, PaneFrameKindV1::End {}))
        ));
        open_connections.push((out, captured));
    }

    assert_eq!(open_connections.len(), 65);
}

/// Overflow before Ready is a visible connection failure, not a token that will later be accepted
/// into silence. The production change that must make this fail is removing the Pending slot while
/// ignoring `EmitOutcome::overflowed` at the connection boundary.
#[test]
fn pending_pane_overflow_visibly_departs_its_connection() {
    let mut lb = Loopback::with_splice_limits(
        "pty-pane-pending-overflow",
        WinSize::new(80, 24),
        super::splice::SpliceLimits::testing_with_subscribers(16, 8, 2, 1),
    );
    let conn = crate::serve::ConnId(1_100);
    let (out, captured) = crate::serve::capture(conn);
    let descriptor = lb
        .host
        .begin_pane_replay(conn, out.clone())
        .expect("the pending replay is reserved");

    lb.host.shared.retain_output(b"a");
    lb.host.shared.retain_output(b"b");

    assert_eq!(
        out.departed(),
        Some(crate::serve::Departure::PaneOverflow {
            agent_id: "node-under-test".into()
        }),
        "pending overflow must name its connection-fatal reason"
    );
    lb.host.pane_ready(conn, &descriptor.token, descriptor.cut);
    assert!(
        captured.try_iter().next().is_none(),
        "overflow must not forge a terminal frame"
    );
    lb.hang_up();
}

#[test]
fn pane_input_failure_cancels_only_the_exact_pending_subscription() {
    let mut lb = Loopback::without_reader("pty-pane-input-failure-pending", WinSize::new(80, 24));
    let failed_conn = crate::serve::ConnId(1_130);
    let healthy_conn = crate::serve::ConnId(1_131);
    let (failed_out, failed_frames) = crate::serve::capture(failed_conn);
    let (healthy_out, healthy_frames) = crate::serve::capture(healthy_conn);
    let failed = lb
        .host
        .begin_pane_replay(failed_conn, failed_out.clone())
        .expect("the failed connection reserves a response-first replay");
    let healthy = lb
        .host
        .begin_pane_replay(healthy_conn, healthy_out)
        .expect("the healthy connection reserves an independent replay");

    lb.host
        .fail_pane_connection(failed_conn, "injected opaque input evidence refusal".into());

    assert_eq!(
        failed_out.departed(),
        Some(crate::serve::Departure::PaneInputFailed {
            agent_id: "node-under-test".into(),
            error: "injected opaque input evidence refusal".into(),
        })
    );
    assert!(
        !lb.host
            .pane_replay_reserved(failed_conn, &failed.token, failed.cut),
        "the failed Pending slot remained activatable"
    );
    assert!(
        lb.host
            .pane_replay_reserved(healthy_conn, &healthy.token, healthy.cut),
        "failing one pane connection disturbed another Pending slot"
    );
    lb.host.pane_ready(failed_conn, &failed.token, failed.cut);
    assert!(
        failed_frames.try_iter().next().is_none(),
        "a cancelled Pending slot forged replay after failure"
    );
    lb.host
        .pane_ready(healthy_conn, &healthy.token, healthy.cut);
    let initial = healthy_frames
        .try_recv()
        .expect("the independent Pending slot remains activatable");
    assert!(matches!(
        Frame::from_line(std::str::from_utf8(&initial).unwrap()).unwrap(),
        Frame::Notification(note)
            if matches!(note.event, Event::NodePaneFrame(ref frame)
                if frame.seq == 0
                    && matches!(frame.frame, PaneFrameKindV1::Resize { cols: 80, rows: 24 }))
    ));
    lb.host.unlisten(healthy_conn);
    lb.hang_up();
}

#[test]
fn pane_input_failure_visibly_cancels_an_in_flight_transition() {
    let mut lb =
        Loopback::without_reader("pty-pane-input-failure-transitioning", WinSize::new(80, 24));
    lb.host.shared.retain_output(b"prefix");
    let failed_conn = crate::serve::ConnId(1_132);
    let healthy_conn = crate::serve::ConnId(1_133);
    let (failed_out, failed_frames) = crate::serve::capture(failed_conn);
    let (healthy_out, healthy_frames) = crate::serve::capture(healthy_conn);
    let failed = lb
        .host
        .begin_pane_replay(failed_conn, failed_out.clone())
        .expect("the failed connection reserves replay");
    let healthy = lb
        .host
        .begin_pane_replay(healthy_conn, healthy_out)
        .expect("the healthy connection reserves replay");
    let (hook, reached, release) = bounded_hook_gate();
    *lb.host
        .shared
        .pane_transition_hook
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(hook);
    lb.host.pane_ready(failed_conn, &failed.token, failed.cut);

    let failed_frames = std::thread::scope(|scope| {
        let driver = scope.spawn(move || failed_frames.try_iter().collect::<Vec<_>>());
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("the replay reached its Transitioning seam");
        lb.host
            .fail_pane_connection(failed_conn, "injected opaque input delivery refusal".into());
        assert_eq!(
            failed_out.departed(),
            Some(crate::serve::Departure::PaneInputFailed {
                agent_id: "node-under-test".into(),
                error: "injected opaque input delivery refusal".into(),
            }),
            "the in-flight transition was cancelled silently"
        );
        assert!(
            lb.host
                .pane_replay_reserved(healthy_conn, &healthy.token, healthy.cut),
            "failing the transition disturbed another Pending slot"
        );
        release.send(()).expect("the replay driver remains alive");
        driver.join().unwrap()
    });
    *lb.host
        .shared
        .pane_transition_hook
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    assert!(failed_frames.iter().all(|line| {
        !matches!(
            Frame::from_line(std::str::from_utf8(line).unwrap()).unwrap(),
            Frame::Notification(note)
                if matches!(note.event, Event::NodePaneFrame(ref frame)
                    if matches!(frame.frame, PaneFrameKindV1::End {}))
        )
    }));

    lb.host
        .pane_ready(healthy_conn, &healthy.token, healthy.cut);
    let initial = healthy_frames
        .try_recv()
        .expect("the independent subscription remains activatable");
    assert!(matches!(
        Frame::from_line(std::str::from_utf8(&initial).unwrap()).unwrap(),
        Frame::Notification(note)
            if matches!(note.event, Event::NodePaneFrame(ref frame)
                if frame.seq == 0
                    && matches!(frame.frame, PaneFrameKindV1::Resize { cols: 80, rows: 24 }))
    ));
    lb.host.unlisten(healthy_conn);
    lb.hang_up();
}

/// Overflow while a Ready record is waiting for its writer is likewise connection-fatal and must
/// remove only that subscriber. The production change that must make this fail is cancelling its
/// Transitioning slot without surfacing a departure.
#[test]
fn ready_pane_overflow_visibly_departs_its_connection() {
    let mut lb = Loopback::with_splice_limits(
        "pty-pane-ready-overflow",
        WinSize::new(80, 24),
        super::splice::SpliceLimits::testing_with_subscribers(16, 8, 2, 1),
    );
    let conn = crate::serve::ConnId(1_101);
    let (out, captured) = crate::serve::capture(conn);
    let descriptor = lb
        .host
        .begin_pane_replay(conn, out.clone())
        .expect("the replay is reserved");
    lb.host.pane_ready(conn, &descriptor.token, descriptor.cut);
    let initial = captured.try_recv().expect("initial geometry is replayed");
    assert!(matches!(
        Frame::from_line(std::str::from_utf8(&initial).unwrap()).unwrap(),
        Frame::Notification(note)
            if matches!(note.event, Event::NodePaneFrame(ref frame)
                if frame.seq == 0
                    && matches!(frame.frame, PaneFrameKindV1::Resize { cols: 80, rows: 24 }))
    ));
    assert!(
        captured.try_iter().next().is_none(),
        "initial replay reaches Ready"
    );

    lb.host.shared.retain_output(b"a");
    lb.host.shared.retain_output(b"b");

    assert_eq!(
        out.departed(),
        Some(crate::serve::Departure::PaneOverflow {
            agent_id: "node-under-test".into()
        }),
        "ready overflow must name its connection-fatal reason"
    );
    assert!(
        captured.try_iter().next().is_none(),
        "overflow must not forge a terminal frame"
    );
    lb.hang_up();
}

/// The writer can consume the splice overflow tombstone and drop its Transitioning slot before
/// the emitter handles `EmitOutcome::overflowed`. That ordering must still visibly fail the
/// connection; relying only on the emitter finding the slot loses the sole `Outbound`.
#[test]
fn transitioning_flow_overflow_departs_when_it_consumes_the_tombstone_first() {
    let mut lb = Loopback::with_splice_limits(
        "pty-pane-transition-overflow-race",
        WinSize::new(80, 24),
        super::splice::SpliceLimits::testing_with_subscribers(16, 8, 2, 1),
    );
    let conn = crate::serve::ConnId(1_106);
    let (out, captured) = crate::serve::capture(conn);
    let descriptor = lb
        .host
        .begin_pane_replay(conn, out.clone())
        .expect("the replay is reserved");
    lb.host.pane_ready(conn, &descriptor.token, descriptor.cut);
    let initial = captured.try_recv().expect("initial geometry is replayed");
    assert!(matches!(
        Frame::from_line(std::str::from_utf8(&initial).unwrap()).unwrap(),
        Frame::Notification(note)
            if matches!(note.event, Event::NodePaneFrame(ref frame)
                if frame.seq == 0
                    && matches!(frame.frame, PaneFrameKindV1::Resize { cols: 80, rows: 24 }))
    ));
    assert!(
        captured.try_iter().next().is_none(),
        "initial replay reaches Ready"
    );

    lb.host.shared.retain_output(b"a");
    let (hook, reached, release) = bounded_hook_gate();
    *lb.host
        .shared
        .pane_splice_outcome_hook
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(hook);

    let (observed, tombstone_was_silent) = std::thread::scope(|scope| {
        let shared = Arc::clone(&lb.host.shared);
        let emitter = scope.spawn(move || shared.retain_output(b"b"));
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("the emitter reached the overflow outcome seam");
        let tombstone_was_silent = captured.try_iter().next().is_none();
        let observed = out.departed();
        release.send(()).expect("the emitter is alive");
        emitter.join().unwrap();
        (observed, tombstone_was_silent)
    });
    *lb.host
        .shared
        .pane_splice_outcome_hook
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    lb.hang_up();

    assert!(
        tombstone_was_silent,
        "the flow forged a frame while consuming the overflow tombstone"
    );
    assert_eq!(
        observed,
        Some(crate::serve::Departure::PaneOverflow {
            agent_id: "node-under-test".into()
        }),
        "flow-side tombstone consumption must retain a connection-fatal path"
    );
}

/// Once unlisten returns, a Flow may finish only a record it had already reserved. A cancellation
/// that wins before `pull_one` must prevent that record from being pulled and written.
#[test]
fn unlisten_linearizes_before_a_not_yet_started_pane_pull() {
    let mut lb = Loopback::without_reader("pty-pane-unlisten-pre-pull", WinSize::new(80, 24));
    lb.host.shared.retain_output(b"must-not-be-pulled");
    let conn = crate::serve::ConnId(1_107);
    let (out, captured) = crate::serve::capture(conn);
    let descriptor = lb
        .host
        .begin_pane_replay(conn, out.clone())
        .expect("the replay is reserved");
    lb.host.pane_ready(conn, &descriptor.token, descriptor.cut);

    let (hook, reached, release) = bounded_hook_gate();
    *lb.host
        .shared
        .pane_pre_pull_hook
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(hook);

    let result = std::thread::scope(|scope| {
        let driver = scope.spawn(move || captured.try_recv());
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("the replay reached its pre-pull seam");
        lb.host.unlisten(conn);
        release.send(()).expect("the replay driver is alive");
        driver.join().unwrap()
    });
    lb.hang_up();

    assert!(
        matches!(result, Err(std::sync::mpsc::TryRecvError::Empty)),
        "unlisten returned before a new replay frame was pulled: {result:?}"
    );
}

/// The cancellation mutex is the reservation boundary: an unlisten that acquires it first stops
/// the next record, while a pull that holds it may finish exactly that already-reserved record.
#[test]
fn pane_pull_holds_the_cancellation_guard_through_cursor_reservation() {
    let mut lb = Loopback::without_reader("pty-pane-guarded-pull", WinSize::new(80, 24));
    lb.host.shared.retain_output(b"reserved-under-guard");
    let conn = crate::serve::ConnId(1_108);
    let (out, captured) = crate::serve::capture(conn);
    let descriptor = lb
        .host
        .begin_pane_replay(conn, out)
        .expect("the replay is reserved");
    lb.host.pane_ready(conn, &descriptor.token, descriptor.cut);

    let cancellation = {
        let streams = lb
            .host
            .shared
            .pane_streams
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        Arc::clone(
            &streams
                .slots
                .get(&conn)
                .expect("the transitioning replay remains registered")
                .cancellation,
        )
    };
    let (hook, reached, release) = bounded_hook_gate();
    *lb.host
        .shared
        .pane_guarded_pull_hook
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(hook);

    let (guarded, result) = std::thread::scope(|scope| {
        let driver = scope.spawn(move || captured.try_recv());
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("the replay reached its guarded-pull seam");
        let guarded = matches!(
            cancellation.cancelled.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        );
        release.send(()).expect("the replay driver is alive");
        (guarded, driver.join().unwrap())
    });
    lb.host.unlisten(conn);
    lb.hang_up();

    assert!(
        guarded,
        "the cancellation mutex was released before cursor reservation"
    );
    assert!(
        result.is_ok(),
        "the guarded replay record was not pulled: {result:?}"
    );
}

/// Pending response-first handshakes are bounded without a timer thread. Advancing an injected
/// clock makes the next reservation prune one stale token, and an exact Ready on another stale
/// token performs the same visible connection failure.
#[test]
fn pending_pane_replays_expire_lazily_on_begin_and_exact_ready() {
    let mut lb = Loopback::with_splice_limits(
        "pty-pane-pending-expiry",
        WinSize::new(80, 24),
        super::splice::SpliceLimits::testing_with_subscribers(16, 8, 1, 4),
    );
    let start = Instant::now();
    let now = Arc::new(std::sync::Mutex::new(start));
    *lb.host
        .shared
        .pane_clock
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Arc::new({
        let now = Arc::clone(&now);
        move || *now.lock().unwrap_or_else(|e| e.into_inner())
    });

    let first_conn = crate::serve::ConnId(1_108);
    let (first_out, first_captured) = crate::serve::capture(first_conn);
    lb.host
        .begin_pane_replay(first_conn, first_out.clone())
        .expect("the first pending replay is reserved");
    *now.lock().unwrap_or_else(|e| e.into_inner()) = start + Duration::from_secs(10);

    let second_conn = crate::serve::ConnId(1_109);
    let (second_out, second_captured) = crate::serve::capture(second_conn);
    let second = lb.host.begin_pane_replay(second_conn, second_out.clone());
    let first_departure = first_out.departed();
    let third_conn = crate::serve::ConnId(1_110);
    let (third_out, _third_captured) = crate::serve::capture(third_conn);
    let (second_departure, third) = if let Some(second) = &second {
        *now.lock().unwrap_or_else(|e| e.into_inner()) = start + Duration::from_secs(20);
        lb.host.pane_ready(second_conn, &second.token, second.cut);
        (
            second_out.departed(),
            lb.host.begin_pane_replay(third_conn, third_out),
        )
    } else {
        (second_out.departed(), None)
    };
    lb.host.unlisten(third_conn);
    lb.hang_up();

    let expired = Some(crate::serve::Departure::PaneReplayExpired {
        agent_id: "node-under-test".into(),
    });
    assert_eq!(first_departure, expired);
    assert!(
        second.is_some(),
        "begin prunes the expired subscriber before enforcing the cap"
    );
    assert_eq!(second_departure, expired);
    assert!(first_captured.try_iter().next().is_none());
    assert!(second_captured.try_iter().next().is_none());
    assert!(
        third.is_some(),
        "exact Ready expiry releases the sole subscriber"
    );
}

/// A host-wide retention failure visibly fails every negotiated pane stream, while legacy live
/// output and the PTY itself continue. The production change that must make this fail is latching
/// the splice error and cancelling slots without waking their connections.
#[test]
fn global_pane_retention_failure_departs_only_opted_in_connections() {
    let mut lb = Loopback::with_splice_limits(
        "pty-pane-global-failure",
        WinSize::new(80, 24),
        super::splice::SpliceLimits::testing_with_subscribers(1, 8, 4, 4),
    );
    let pending_conn = crate::serve::ConnId(1_102);
    let ready_conn = crate::serve::ConnId(1_103);
    let legacy_conn = crate::serve::ConnId(1_104);
    let (pending_out, pending_rx) = crate::serve::capture(pending_conn);
    let (ready_out, ready_rx) = crate::serve::capture(ready_conn);
    let (legacy_out, legacy_rx) = crate::serve::capture(legacy_conn);
    let _pending = lb
        .host
        .begin_pane_replay(pending_conn, pending_out.clone())
        .expect("pending replay");
    let ready = lb
        .host
        .begin_pane_replay(ready_conn, ready_out.clone())
        .expect("ready replay");
    lb.host.pane_ready(ready_conn, &ready.token, ready.cut);
    let initial = ready_rx.try_recv().expect("initial geometry is replayed");
    assert!(matches!(
        Frame::from_line(std::str::from_utf8(&initial).unwrap()).unwrap(),
        Frame::Notification(note)
            if matches!(note.event, Event::NodePaneFrame(ref frame)
                if frame.seq == 0
                    && matches!(frame.frame, PaneFrameKindV1::Resize { cols: 80, rows: 24 }))
    ));
    assert!(ready_rx.try_iter().next().is_none());
    lb.host.listen(legacy_out.clone());

    lb.host.shared.retain_output(b"too large");

    for out in [&pending_out, &ready_out] {
        assert!(
            matches!(
                out.departed(),
                Some(crate::serve::Departure::PaneRetentionFailed { ref agent_id, .. })
                    if agent_id == "node-under-test"
            ),
            "retention failure was silent for connection {}: {:?}",
            out.conn().0,
            out.departed()
        );
    }
    assert_eq!(legacy_out.departed(), None);
    lb.host.shared.emit("legacy-continues");
    assert!(legacy_rx.try_recv().is_ok(), "legacy live output stopped");
    assert!(pending_rx.try_iter().next().is_none());
    assert!(ready_rx.try_iter().next().is_none());
    assert!(
        lb.host
            .begin_pane_replay(crate::serve::ConnId(1_105), legacy_out)
            .is_none(),
        "failed retention minted a future exact replay"
    );
    lb.hang_up();
}

#[test]
fn pty_retention_overflow_latches_without_blocking_cast_or_live_output() {
    let mut lb = Loopback::with_splice_limits(
        "pty-retention-overflow",
        WinSize::new(80, 24),
        super::splice::SpliceLimits::testing(3, 8),
    );
    let (listener, rx) = crate::serve::capture(crate::serve::ConnId(91));
    lb.host.listen(listener);

    lb.child_writes(b"ok");
    lb.child_writes(b"\xffboom");
    let first_error = lb
        .host
        .retention_error()
        .expect("the first over-limit output is latched");
    assert!(first_error.contains("3-byte limit"), "{first_error}");
    let frozen = lb.host.retention_snapshot();
    assert_eq!(frozen.len(), 1);

    lb.child_writes(b"later");
    assert_eq!(
        lb.host.retention_error().as_deref(),
        Some(first_error.as_str())
    );
    assert_eq!(lb.host.retention_snapshot(), frozen);
    // **Wait on the frames, not on the counter.** `Shared::emit` bumps `seq` *before* it fans the
    // event out to the listeners, so `next_seq() == 3` says only that the third write entered
    // `emit` — under scheduling pressure the third frame can still be unsent when a drain that
    // trusted the counter reads the channel, and the run fails on a missing `later`. The delivered
    // frames are the event this test is actually about, so accumulate them until they say what the
    // third write says.
    let mut live = String::new();
    let delivered = until(|| {
        for frame in rx.try_iter() {
            let frame: serde_json::Value = serde_json::from_slice(&frame).unwrap();
            assert_eq!(frame["method"], "node/pty");
            live.push_str(frame["params"]["bytes"].as_str().unwrap());
        }
        live == "ok\u{fffd}boomlater"
    });
    assert!(
        delivered,
        "live output stopped after retention failed: {live:?}"
    );
    assert_eq!(lb.host.next_seq(), 3);

    lb.hang_up();
    lb.host.shutdown().unwrap();
    let (_, records) = read_cast(&lb.cast);
    let cast_output: String = records
        .iter()
        .filter(|(_, code, _)| code == "o")
        .map(|(_, _, data)| data.as_str())
        .collect();
    assert_eq!(cast_output, live);
    assert_eq!(lb.host.retention_snapshot(), frozen);
}

#[test]
fn pty_resize_retention_overflow_does_not_rollback_cast_or_output() {
    let mut lb = Loopback::with_splice_limits(
        "pty-retention-resize-overflow",
        WinSize::new(80, 24),
        super::splice::SpliceLimits::testing(64, 1),
    );
    lb.child_writes(b"before");
    lb.host.resize(WinSize::new(100, 40)).unwrap();
    let first_error = lb
        .host
        .retention_error()
        .expect("the over-limit resize is latched");
    assert!(first_error.contains("record limit 1"), "{first_error}");
    lb.child_writes(b"after");
    lb.hang_up();
    lb.host.shutdown().unwrap();

    let retained = lb.host.retention_snapshot();
    assert_eq!(retained.len(), 1);
    assert!(matches!(
        &retained[0].kind,
        super::splice::DisplayKind::Output(bytes) if bytes.as_ref() == b"before"
    ));
    assert_eq!(
        lb.host.retention_error().as_deref(),
        Some(first_error.as_str())
    );

    let (_, records) = read_cast(&lb.cast);
    let observed: Vec<_> = records
        .iter()
        .filter(|(_, code, _)| code == "o" || code == "r")
        .map(|(_, code, data)| (code.as_str(), data.as_str()))
        .collect();
    assert_eq!(observed, [("o", "before"), ("r", "100x40"), ("o", "after")]);
}

#[test]
fn pty_live_resize_retains_kernel_success_before_reporting_cast_failure() {
    let mut lb = Loopback::new("pty-retention-live-cast-failure", WinSize::new(80, 24));
    lb.child_writes(b"before");
    lb.host
        .fail_next_cast_resize("injected live cast resize failure");
    let error = lb
        .host
        .resize(WinSize::new(100, 40))
        .expect_err("the injected cast failure reaches the caller");
    assert!(
        error
            .to_string()
            .contains("injected live cast resize failure")
    );
    lb.child_writes(b"after");

    let retained = lb.host.retention_snapshot();
    assert_eq!(
        retained.iter().map(|record| record.seq).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(matches!(
        retained[1].kind,
        super::splice::DisplayKind::Resize {
            rows: 40,
            cols: 100
        }
    ));

    lb.hang_up();
    lb.host.shutdown().unwrap();
}

#[test]
fn pty_no_reader_resize_retains_kernel_success_before_reporting_cast_failure() {
    let mut lb =
        Loopback::without_reader("pty-retention-no-reader-cast-failure", WinSize::new(80, 24));
    lb.host.shared.retain_output(b"before");
    lb.host
        .fail_next_cast_resize("injected fallback cast resize failure");
    let error = lb
        .host
        .resize(WinSize::new(100, 40))
        .expect_err("the injected cast failure reaches the caller");
    assert!(
        error
            .to_string()
            .contains("injected fallback cast resize failure")
    );
    lb.host.shared.retain_output(b"after");

    let retained = lb.host.retention_snapshot();
    assert_eq!(
        retained.iter().map(|record| record.seq).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(matches!(
        retained[1].kind,
        super::splice::DisplayKind::Resize {
            rows: 40,
            cols: 100
        }
    ));

    lb.hang_up();
    lb.host.shutdown().unwrap();
}

#[test]
fn pty_ioctl_failure_never_retains_resize_in_either_path() {
    let mut live = Loopback::new("pty-retention-live-ioctl-failure", WinSize::new(80, 24));
    live.host
        .fail_next_resize_ioctl("injected live ioctl failure");
    assert!(live.host.resize(WinSize::new(100, 40)).is_err());
    assert!(live.host.retention_snapshot().is_empty());
    live.hang_up();
    live.host.shutdown().unwrap();

    let mut fallback = Loopback::without_reader(
        "pty-retention-no-reader-ioctl-failure",
        WinSize::new(80, 24),
    );
    fallback
        .host
        .fail_next_resize_ioctl("injected fallback ioctl failure");
    assert!(fallback.host.resize(WinSize::new(100, 40)).is_err());
    assert!(fallback.host.retention_snapshot().is_empty());
    fallback.hang_up();
    fallback.host.shutdown().unwrap();
}

/// **A resize returns even when the reader thread never performs it.**
///
/// `PtyHost::resize` hands the ioctl and the `r` record to the reader thread and waits for the
/// answer, which is what makes the record truthful. The wait itself, though, is a claim about
/// another thread's liveness made on a *client connection thread* — `handler`'s `node/resize` runs
/// there, and `serve.rs`'s accept loop joins those threads before the supervisor can stop. An
/// unbounded wait therefore does not cost one resize: it costs the shutdown, and a suite that
/// exercises the path (`handler::tests::a_resize_reaches_the_pty_and_the_child_is_told`) stops
/// being able to fail and can only hang.
///
/// The stall is caused, not timed. The resize hook runs **on the reader thread** inside
/// `apply_pending_resize`, so `reached` proves the reader is inside the very function the caller is
/// waiting on before the caller's outcome is sampled — no sleep stands in for that ordering. The
/// sample itself is bounded so a reintroduced `Condvar::wait` reports `Timeout` here instead of
/// wedging the harness, and the hook is released on every path so teardown cannot inherit the
/// stall.
#[test]
fn a_resize_returns_even_when_the_reader_never_performs_it() {
    let mut lb = Loopback::new("pty-resize-stalled-reader", WinSize::new(80, 24));
    let (release, held) = std::sync::mpsc::sync_channel::<()>(0);
    let (entered, reached) = std::sync::mpsc::sync_channel::<()>(1);
    let held = std::sync::Mutex::new(held);
    lb.host.set_resize_hook(Box::new(move || {
        let _ = entered.try_send(());
        let _ = held.lock().unwrap_or_else(|e| e.into_inner()).recv();
    }));

    let outcome = std::thread::scope(|scope| {
        let (finished, answered) = std::sync::mpsc::sync_channel::<io::Result<()>>(1);
        let host = &lb.host;
        scope.spawn(move || {
            let _ = finished.send(host.resize(WinSize::new(100, 30)));
        });
        reached
            .recv_timeout(Duration::from_secs(10))
            .expect("the reader reached the resize it is supposed to perform");
        // Generously past `RESIZE_APPLY_GRACE`, so this bound can only expire on a resize that has
        // no bound of its own.
        let outcome = answered.recv_timeout(Duration::from_secs(20));
        // Before the assertion, so the reader is freed whichever way the sample went.
        let _ = release.send(());
        outcome
    });

    assert!(
        matches!(outcome, Ok(Ok(()))),
        "resize never returned while the reader thread was stalled inside it, so a client \
         connection thread — and with it the accept loop that joins it — can be wedged by a pty \
         reader that is merely slow: {outcome:?}"
    );
    assert_eq!(
        lb.host.master().size().expect("TIOCGWINSZ"),
        WinSize::new(100, 30),
        "the caller returned without the kernel ever being resized, which would make the answer a \
         lie rather than a fallback"
    );

    lb.host.clear_resize_hook();
    lb.hang_up();
    lb.host.shutdown().unwrap();
}

/// **No pty byte reaches `events.jsonl`.** §3.4 said they would; they do not, and §3.4 has been
/// corrected. Recorded here as a decision so it cannot be undone by accident.
///
/// **Mutation:** route the reader's decoded text into `EventWriter::record` as well as into the
/// cast.
#[test]
fn no_pty_byte_reaches_events_jsonl() {
    use crate::events::EventWriter;

    let dir = marion_testsupport::scratch("pty-not-events");
    let events = dir.join("events.jsonl");
    let w = EventWriter::open_path(&events, &AgentId("split".into())).unwrap();
    let origin = w.origin();
    drop(w);
    let before = std::fs::read_to_string(&events).unwrap_or_default();

    let master = PtyMaster::open(WinSize::new(80, 24)).unwrap();
    let master_slave = master.open_slave().unwrap();
    let host = PtyHost::start(
        AgentId("split".into()),
        master,
        &dir.join("pty.cast"),
        WinSize::new(80, 24),
        "xterm-256color",
        origin,
    )
    .unwrap();
    {
        let mut slave = std::fs::File::from(master_slave);
        let payload: &[u8] = b"\x1b[?1049h PTY BYTES \x1b[2J";
        slave.write_all(payload).unwrap();
        assert!(
            until(|| host.bytes_read() >= payload.len() as u64),
            "the host read {} of {} bytes",
            host.bytes_read(),
            payload.len()
        );
    }
    host.shutdown().unwrap();

    let after = std::fs::read_to_string(&events).unwrap_or_default();
    assert_eq!(
        after, before,
        "`events.jsonl` grew. §3.4's original wording is wrong three times over: `Payload::Raw` is \
         defined as a *line of stdout*, `EventReader::open_path` reads the whole file on every \
         attach, and `event.rs:40` says `mono_ns` exists to align the two files rather than merge \
         them."
    );
    let (_, records) = read_cast(&dir.join("pty.cast"));
    assert!(
        records
            .iter()
            .any(|(_, c, d)| c == "o" && d.contains("PTY BYTES")),
        "the bytes went somewhere, and `pty.cast` is where"
    );
}

// ---------------------------------------------------------------------------------------------
// probes and shutdown
// ---------------------------------------------------------------------------------------------

/// Probes are **counted, never answered** — §9's M3 criterion de-scopes answering, and
/// `s2/ptyhost.py` answers nothing yet drove complete sessions on both harnesses. The counter is
/// the diagnosability half: if §11 item 9's `ESC[6n` stall ever bites, an operator sees "3
/// unanswered probes" beside a hung node instead of an unexplained hang.
///
/// The split-across-reads case is asserted because it is the same MUST again: a probe is five bytes
/// and a pty read boundary does not care.
#[test]
fn probes_are_counted_across_read_boundaries_and_never_answered() {
    let mut lb = Loopback::new("pty-probes", WinSize::new(80, 24));
    // DA1, XTVERSION, CPR, OSC 10 — §5.3's table, both harnesses' unions.
    lb.child_writes(b"\x1b[c");
    lb.child_writes(b"\x1b[>0q");
    // CPR, split so no single read contains it.
    lb.child_writes(b"\x1b[6");
    lb.child_writes(b"n");
    lb.child_writes(b"\x1b]10;?\x07");
    assert!(
        until(|| lb.host.unanswered_probes() == 4),
        "saw {} probes, wanted 4",
        lb.host.unanswered_probes()
    );

    // Nothing was written back. A responder would have put bytes into the master, which the slave
    // would be able to read; the slave is still at end of its own input.
    lb.hang_up();
    lb.host.shutdown().unwrap();
    let (_, records) = read_cast(&lb.cast);
    assert!(
        !records.iter().any(|(_, c, _)| c == "i"),
        "marion answered a probe; §9 de-scopes answering for M3 and the captures show it is not \
         needed: {records:?}"
    );
}

/// **Kill and reap before closing the master**, because the alternative writes a false cause of
/// death into every live node's record.
///
/// The child here is killed by marion. If the master were closed first, the kernel would SIGHUP it
/// and the recorded death would be signal 1 — "the terminal hung up" — when the truth is that
/// marion decided to stop. That is a truthfulness requirement, not a tidiness one.
///
/// **Mutation:** in `PtyHost::shutdown`, drop the master (or join the reader, which releases the
/// last `Arc`) before killing the child.
#[test]
fn shutdown_kills_and_reaps_before_closing_the_master() {
    use std::os::unix::process::ExitStatusExt;

    let dir = marion_testsupport::scratch("pty-shutdown");
    let master = PtyMaster::open(WinSize::new(80, 24)).unwrap();
    let host = PtyHost::start(
        AgentId("shutdown".into()),
        master,
        &dir.join("pty.cast"),
        WinSize::new(80, 24),
        "xterm-256color",
        Instant::now(),
    )
    .unwrap();
    let child = spawn_pty(
        witness(),
        &mut sh("exec sleep 30"),
        host.master(),
        StdinPlan::TerminalSlave,
        None,
    )
    .unwrap();
    let pid = child.pid();
    host.adopt(child);
    assert!(until(|| alive(pid)));
    std::thread::sleep(Duration::from_millis(50));

    let status = host.shutdown().unwrap().expect("reaped");
    assert_eq!(
        status.signal(),
        Some(9),
        "SIGKILL from marion. SIGHUP (signal 1) here would mean the master closed first, and every \
         pty node's record would read `the terminal hung up` when marion is what stopped it."
    );
    assert!(
        until(|| !alive(pid)),
        "reaped, so no untracked live process"
    );

    let (_, records) = read_cast(&dir.join("pty.cast"));
    assert_eq!(
        records.last().map(|(_, c, d)| (c.as_str(), d.as_str())),
        Some(("x", "signal 9")),
        "the recording says what actually killed it"
    );
}

/// **A registered host is still a reapable one, and this is the assertion the launch path rests
/// on.**
///
/// The blocker that kept `root::launch_terminal` unwritten for four readings of this seam was
/// stated as an impossibility: `RegistryHandle::register_pane` needs `Arc<PtyHost>`, `shutdown`
/// needed `&mut PtyHost`, and *"it cannot be both"*. It can — the two owners are the launcher that
/// must reap and the registry that must serve, and what they actually need is interior mutability
/// on two fields, not exclusive access to the host. Nothing about the ordering `shutdown` promises
/// changes; what changes is who may ask for it.
///
/// The second clone is held **across** the shutdown on purpose: that is the registry's entry, still
/// answering `node/attach` while the launcher reaps, and it is the exact configuration a `&mut`
/// signature made unrepresentable.
///
/// `/bin/sleep` and not `sh -c 'sleep 30'`: this asserts marion killed the process, and a shell
/// standing between marion and the thing being signalled is how a whole mutation went invisible
/// here once already.
///
/// **Mutation:** take `self.child` by reference in `shutdown` instead of `take()`-ing it, then have
/// the second owner call `resize` from another thread while the child is being waited on — the
/// resize parks behind the reap. Asserted below by doing exactly that.
#[test]
fn a_host_two_owners_hold_is_still_reaped_and_still_serves_its_second_owner() {
    use std::os::unix::process::ExitStatusExt;

    let dir = marion_testsupport::scratch("pty-arc-shutdown");
    let master = PtyMaster::open(WinSize::new(80, 24)).unwrap();
    let host = Arc::new(
        PtyHost::start(
            AgentId("arc".into()),
            master,
            &dir.join("pty.cast"),
            WinSize::new(80, 24),
            "xterm-256color",
            Instant::now(),
        )
        .unwrap(),
    );
    let child = spawn_pty(
        witness(),
        Command::new("/bin/sleep").arg("30"),
        host.master(),
        StdinPlan::TerminalSlave,
        None,
    )
    .unwrap();
    let pid = child.pid();
    // Adoption through a shared reference: this is `launch_terminal`'s call, and it does not own
    // the host exclusively either.
    host.adopt(child);

    // The registry's copy, taken before the reap and released after it.
    let registered = Arc::clone(&host);
    assert!(until(|| alive(pid)));

    // A client typing and resizing through the *other* owner, concurrently with the reap. Both must
    // return rather than deadlock against the child lock.
    let resizer = {
        let registered = Arc::clone(&registered);
        std::thread::spawn(move || {
            for i in 0..40 {
                let _ = registered.resize(WinSize::new(80 + i % 3, 24));
            }
        })
    };

    let status = host.shutdown().unwrap().expect("reaped");
    assert_eq!(
        status.signal(),
        Some(9),
        "the ordering guarantee is unchanged by the ownership change: SIGKILL from marion, not \
         SIGHUP from a master that closed first"
    );
    resizer
        .join()
        .expect("a resize from the second owner deadlocked against the reap");
    assert!(until(|| !alive(pid)), "pid {pid} was not reaped");
    assert_eq!(
        registered.child_pid(),
        None,
        "the registry's copy still names a child marion has already reaped, so a later resize \
         would signal a pid the kernel is free to have reissued"
    );
    drop(registered);
}

/// A host dropped without `shutdown` still leaves no live child. §9's M2 criterion forbids an
/// untracked live process, and a leaked pty child is exactly one.
#[test]
fn dropping_a_host_leaves_no_child_behind() {
    let dir = marion_testsupport::scratch("pty-drop");
    let master = PtyMaster::open(WinSize::new(80, 24)).unwrap();
    let host = PtyHost::start(
        AgentId("drop".into()),
        master,
        &dir.join("pty.cast"),
        WinSize::new(80, 24),
        "xterm-256color",
        Instant::now(),
    )
    .unwrap();
    let child = spawn_pty(
        witness(),
        &mut sh("exec sleep 30"),
        host.master(),
        StdinPlan::TerminalSlave,
        None,
    )
    .unwrap();
    let pid = child.pid();
    host.adopt(child);
    assert!(until(|| alive(pid)));
    drop(host);
    assert!(until(|| !alive(pid)), "pid {pid} outlived its host");
}

// ---------------------------------------------------------------------------------------------
// The write half is leased to exactly one client
// ---------------------------------------------------------------------------------------------

/// **Mutation: let `lease_writer` hand every caller a lease.**
///
/// Reading a node fans out — any number of clients may listen, because a byte delivered twice
/// costs nothing. Writing does not. Two clients typing into one pty interleave at whatever
/// granularity their reads happen to have, and the failure is **silent in both directions**: the
/// pty echoes the mangled result back to both operators identically, so neither can tell it from
/// the harness misbehaving.
///
/// This is the test that makes the second attacher's refusal observable. Note what it asserts: not
/// merely that the second call fails, but that it names the holder — a client told only "busy"
/// cannot distinguish a colleague in the same node from a lease that leaked.
#[test]
fn a_second_attacher_is_refused_the_write_half_by_name() {
    let lb = Loopback::new("pty-one-writer", WinSize::new(80, 24));
    assert_eq!(lb.host.writer(), None, "a fresh host has no writer");

    let first = lb
        .host
        .lease_writer(crate::serve::ConnId(7))
        .expect("the first attacher gets the write half");
    assert_eq!(lb.host.writer(), Some(crate::serve::ConnId(7)));

    match lb.host.lease_writer(crate::serve::ConnId(9)) {
        Err(crate::pty::WriterBusy::HeldBy(owner)) => assert_eq!(
            owner,
            crate::serve::ConnId(7),
            "the refusal named the wrong connection"
        ),
        Ok(_) => panic!(
            "a second attacher silently got the write half — two operators are now typing \
             into one pty and neither will see an error"
        ),
    }

    // Re-claiming from the holder is also refused: two live leases for one connection would each
    // clear the slot on drop, and the first drop would open the node to a third client while the
    // second lease was still in use.
    assert!(
        lb.host.lease_writer(crate::serve::ConnId(7)).is_err(),
        "the holder was issued a second lease, so dropping either one opens the node"
    );
    drop(first);
}

/// **Mutation: remove `impl Drop for WriteLease`.**
///
/// §7.3.1 is about what a *crashed* client leaves behind. A node whose one writer died without
/// releasing the lease would be permanently read-only — recoverable only by restarting the
/// supervisor, which is the one thing a client crash must never require.
#[test]
fn a_departed_writer_releases_the_half_without_anybody_cleaning_up() {
    let lb = Loopback::new("pty-writer-drop", WinSize::new(80, 24));
    {
        let _lease = lb
            .host
            .lease_writer(crate::serve::ConnId(1))
            .expect("first");
        assert!(lb.host.lease_writer(crate::serve::ConnId(2)).is_err());
    } // the client goes away — no explicit release anywhere
    assert_eq!(
        lb.host.writer(),
        None,
        "the write half was never handed back, so this node can no longer be typed into"
    );
    let second = lb.host.lease_writer(crate::serve::ConnId(2));
    assert!(
        second.is_ok(),
        "the next attacher could not take the write half"
    );
}

/// A lease is proof about **this** node, not about pty-ness in general. Without the identity
/// check, a client holding one terminal node's write half could type into every other one.
#[test]
fn a_lease_from_another_node_cannot_be_used_to_type_into_this_one() {
    let a = Loopback::new("pty-lease-a", WinSize::new(80, 24));
    let b = Loopback::new("pty-lease-b", WinSize::new(80, 24));

    let lease_for_a = a.host.lease_writer(crate::serve::ConnId(1)).expect("a");
    let err = b
        .host
        .write_input(&lease_for_a, b"rm -rf /\r")
        .expect_err("b accepted a's lease");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);

    // And the control: the lease does work on the node it was issued for.
    a.host
        .write_input(&lease_for_a, b"ok\r")
        .expect("a's own lease");
}

/// Listening stays fan-out. The lease constrains **input** and must not have quietly made the
/// read side exclusive too, which would break every second viewer of a node.
#[test]
fn leasing_the_write_half_does_not_make_reading_exclusive() {
    let lb = Loopback::new("pty-read-fanout", WinSize::new(80, 24));
    let _lease = lb
        .host
        .lease_writer(crate::serve::ConnId(1))
        .expect("writer");
    let (a, _a_rx) = crate::serve::capture(crate::serve::ConnId(1));
    let (b, _b_rx) = crate::serve::capture(crate::serve::ConnId(2));
    lb.host.listen(a);
    lb.host.listen(b);
    assert_eq!(
        lb.host.listeners(),
        2,
        "a read-only second attacher must still receive the stream"
    );
}

// ---------------------------------------------------------------------------------------------
// What still stands between this module and production
// ---------------------------------------------------------------------------------------------

/// **The seam is one level above `launch_path`, and this pins where.**
///
/// I4's brief locates the gap at `duplex::launch_path`, which answers `None` for `TerminalInput`
/// — the reading being that a third `LaunchPath` arm is all that stands between this module and a
/// live pty. **Measured, that is not the blocker.** `spawn_pty` cannot be called without a
/// `PtyWitness`, `PtyWitness` comes only from `ExecutionSurfaces::display_plane`, and that is
/// `Some` iff `display == NativePty` — which is the *display* axis, not the control axis
/// `launch_path` branches on. Adding a `LaunchPath::Terminal` arm would therefore change nothing:
/// even the `Duplex` path cannot obtain a witness today.
///
/// The actual state, asserted rather than described: **no built-in adapter declares a display
/// plane at all.** `claude` is `headless(StreamJson)` (`StructuredUi`) and the other three are
/// `launch_only_with_protocol_events()` (`None`). `ExecutionSurfaces::shared` — the one preset
/// that yields a witness while still speaking a typed protocol over pipes, and the case
/// [`stdin_plan`]'s exhaustive match was written for — is constructed nowhere outside tests.
///
/// **This paragraph used to say the change was a one-line edit in an adapter, and that is false.**
/// It is recorded here rather than deleted because it has now misdirected three readers.
///
/// The one line is real: `ClaudeCodeAdapter::surfaces` returning
/// `ExecutionSurfaces::shared(TypedKind::StreamJson)` instead of `headless` does mint the witness,
/// and `stdin_plan` does then answer `Piped` (a `shared` node still speaks stream-json over a pipe,
/// which is exactly why the two axes are separate). What the paragraph got wrong is *"everything
/// downstream already exists"*. Three things do not:
///
/// 1. **Nothing constructs a `PtyHost` on a launch path.** The supervisor now has somewhere to put
///    one — `RegistryHandle::register_pane`, and `node/attach` leases the keyboard and fans the
///    bytes out — but `root::launch_duplex` and `run::launch_only_child` still spawn with three
///    pipes, so the map is filled by nobody.
/// 2. **`shared` puts the *protocol* stream on the pty.** `spawn_pty` under `StdinPlan::Piped`
///    hands the slave to fd 1 *and* fd 2, which is the topology `shared` means. `duplex::run_duplex`
///    reads stream-json off `child.stdout` as a pipe with a `BufReader::lines`, and under a pty
///    that fd is the slave — read by `PtyHost`'s own thread, which is the single reader of the
///    master. Two readers on one stream split frames, so the protocol driver would have to be fed
///    from the pty instead. That is a real change to the most-measured file in this repo.
/// 3. **stderr merges into the frame stream.** This is the part that makes it a hazard rather than
///    a refactor. On the pipe topology `duplex` drains stderr separately; on the pty topology both
///    land on one file description, so a diagnostic written mid-line lands *inside* a JSON frame
///    M1's reader must parse. `DuplexOutcome.stderr` would also always be empty.
///
/// The protocol itself survives the pty, which was the open question and is now measured rather
/// than assumed: **claude 2.1.224 with `--print --output-format stream-json --input-format
/// stream-json` and its stdout on a pty slave still emits stream-json** (28 580 bytes over one
/// turn, `system` frames first), with every `\n` post-processed to `\r\n` by ONLCR — 22 line
/// terminators, 22 of them CRLF. `\r` is JSON whitespace, so a line-oriented parser survives it.
/// So the objection to flipping the surface is not "it stops working"; it is item 3.
///
/// The edit is therefore still **not** made, and now for a stated reason rather than a deferral:
/// it changes how the root node is launched on the one path M1 and M2's criteria are measured
/// through, in a way that puts an unmeasured stderr-interleaving hazard inside M1's protocol
/// reader. When it is made it should be **per run** — a node gets a pane because a client asked
/// for one, not because every `claude` node everywhere now launches differently — so that M1's
/// measured path stays byte-identical for a run that wants no pane.
///
/// **RESOLVED, and this test survives the resolution as the guard it should always have been.**
///
/// The prediction in the paragraph above — that flipping `ClaudeCodeAdapter::surfaces` to `shared`
/// is what mints the witness — was the fourth reading of this seam and is also wrong, for a reason
/// §9's own M3 criteria state: *"a real `claude` TUI runs in a marion pane"* and *"a real `codex`
/// TUI runs in a pane with scrollback retained across at least one resize"*. **M3 is not about the
/// headless node at all.** It is about a node marion drives by keystrokes — §3.4's `opaque` — which
/// has **no frame parser**, so hazards 2 and 3 above are absent by construction rather than
/// mitigated. `shared` was never the shape to reach for.
///
/// So the pane is `HarnessAdapter::pane_surfaces`, a *second* surfaces/compile pair asked for per
/// run, and `surfaces()` keeps declaring no display plane. **This assertion is therefore still
/// true, and it is now the test for the thing it should always have been testing: a node that did
/// not ask for a pane must not get one.** A run with no pane request is launched byte-identically
/// to how it was before any of this existed, which is what keeps M1's measured path measured.
///
/// Mutation: make `ClaudeCodeAdapter::surfaces` return `pane_surfaces().unwrap()`. This fails.
#[test]
fn a_node_that_did_not_ask_for_a_pane_is_not_given_one() {
    use marion_core::harness::Harness;
    use marion_harness::adapter_for;

    for h in Harness::ALL {
        let surfaces = adapter_for(h).expect("a built-in adapter").surfaces();
        assert!(
            surfaces.display_plane().is_none(),
            "{h:?} now declares a display plane. PtyHost is reachable from production for the \
             first time, so `run_spawn`/`root` must construct one (PtyMaster::open -> \
             PtyHost::start -> spawn_pty with the witness, stdin from `stdin_plan`) and hand it \
             to `RegistryHandle::register_pane`, which is where `node/attach` looks. The attach \
             half is built; the launch half is not. If this is a `Typed(_)` surface, read the \
             doc above first: the protocol stream moves onto the pty with the display, and \
             stderr moves onto it too. Update this test in the same commit that does so."
        );
    }
}

/// The same rule on the control axis, and it is the S11 MUST #2 guard.
///
/// `stdin_plan` is a total match on `ControlTransport`, so **whichever axis a node's surfaces
/// declare decides its stdin and there is no third answer**: `Typed(_)` is `Piped`, and S11
/// measured `claude -p` exiting 1 with *"Input must be provided…"* when it is not. A default
/// surface that drifted to `TerminalInput` would hand a headless harness the slave on fd 0.
///
/// `duplex::launch_path` now has the third arm this used to predict (`LaunchPath::Terminal`), and
/// it is reached only from `pane_surfaces()`. This asserts the *default* shapes still are not.
///
/// Mutation: make any adapter's `surfaces()` return `ExecutionSurfaces::opaque()`. This fails,
/// and so does `the_typed_control_axis_keeps_its_piped_stdin` below.
#[test]
fn no_built_in_agent_types_default_shape_is_driven_by_keystrokes() {
    use marion_core::harness::Harness;
    use marion_harness::{ControlTransport, adapter_for};

    let terminal: Vec<Harness> = Harness::ALL
        .into_iter()
        .filter(|h| {
            adapter_for(*h)
                .expect("a built-in adapter")
                .surfaces()
                .control
                == ControlTransport::TerminalInput
        })
        .collect();
    assert!(
        terminal.is_empty(),
        "{terminal:?} declare TerminalInput as their *default* shape, so every node of that \
         harness — including M1's — would be given the pty slave on fd 0 by `stdin_plan`, which \
         S11 measured `claude -p` exiting 1 over. A pane is asked for per run through \
         `HarnessAdapter::pane_surfaces`, never declared for every run here"
    );
}

/// **S11 MUST #2, asserted through the total match rather than around it.**
///
/// `PtyWitness` alone does not keep a typed node off a pty stdin — the `shared` preset is the
/// counterexample, and it is exactly the preset a reader flipping an adapter would reach for. This
/// is the second half: whatever the display axis says, a `Typed(_)` control axis is `Piped`.
///
/// Mutation: add a `Typed(_) => StdinPlan::TerminalSlave` arm, or widen the match with a `_` that
/// answers `TerminalSlave`. This fails.
#[test]
fn the_typed_control_axis_keeps_its_piped_stdin_whatever_the_display_axis_says() {
    use marion_harness::{ControlTransport, DisplaySurface, ExecutionSurfaces, TypedKind};

    for kind in [TypedKind::StreamJson, TypedKind::AppServer, TypedKind::Acp] {
        // Both points of the display axis a typed node can sit at, including the one that yields
        // a witness. The witness is what makes this worth asserting: a caller holding one has
        // everything `spawn_pty` needs *except* the right stdin.
        for display in [DisplaySurface::NativePty, DisplaySurface::StructuredUi] {
            let s = ExecutionSurfaces::new(ControlTransport::Typed(kind), display, []);
            assert_eq!(
                stdin_plan(s.control),
                StdinPlan::Piped,
                "{s:?}: S11 measured `claude -p` exiting 1 on an isatty(0) stdin"
            );
        }
    }
    // And the two that are not typed, so this cannot pass by answering `Piped` unconditionally.
    assert_eq!(
        stdin_plan(marion_harness::ControlTransport::TerminalInput),
        StdinPlan::TerminalSlave
    );
    assert_eq!(
        stdin_plan(marion_harness::ControlTransport::LaunchOnly),
        StdinPlan::Null
    );
}

/// **The pane shape's stdin is the slave, and its surfaces are what say so.**
///
/// The pairing is the whole of why a pane is safe: `TerminalInput` selects `TerminalSlave`, which
/// is the one plan under which `spawn_pty` hands fd 0 the slave *and* issues `TIOCSCTTY` on fd 0.
#[test]
fn the_pane_shape_of_every_harness_that_has_one_asks_for_the_slave_on_stdin() {
    use marion_core::harness::Harness;
    use marion_harness::adapter_for;

    let mut seen = 0;
    for h in Harness::ALL {
        let Some(s) = adapter_for(h).expect("a built-in adapter").pane_surfaces() else {
            continue;
        };
        seen += 1;
        assert!(
            s.display_plane().is_some(),
            "{h:?}: a pane shape must mint the witness `spawn_pty` requires"
        );
        assert_eq!(stdin_plan(s.control), StdinPlan::TerminalSlave, "{h:?}");
    }
    assert!(seen > 0, "no harness declares a pane shape at all");
}

// ---------------------------------------------------------------------------------------------
// The window between `spawn()` and somebody taking responsibility
// ---------------------------------------------------------------------------------------------

/// **A panicking `on_started` hook must not leave a live, un-reaped child.**
///
/// `spawn_pty` announces the pid before the caller can do anything with the handle — topology point
/// 7, and it has to be that early, because `run.rs` reads the process's start identity there while
/// marion still holds the `Child` and that read is only race-free while it does. The hook is
/// therefore arbitrary caller code running at the one instant nothing else owns the process.
///
/// It used to run *before* the `Child` was wrapped, and `std::process::Child`'s own `Drop`
/// **neither kills nor reaps**. So an unwind through the hook dropped the only handle to a live
/// process on a pty nothing was reading: no `PtyHost` had adopted it, no `wait` could ever be
/// issued for it, and its pid had already been announced to an owner that was about to be told the
/// spawn failed. That is §11 item 30's untracked live process reached by a panic rather than by a
/// crash — and unlike a crash, `catch_unwind` means the process goes on running afterwards.
///
/// `!alive(pid)` is the reaped assertion and not merely the dead one: `kill(pid, 0)` succeeds
/// against a **zombie**, so an `ESRCH` here means the wait really happened and the entry is gone.
///
/// **Mutation:** move the `PtyChild` construction back below the `on_started` call in `spawn_pty`,
/// or delete `impl Drop for PtyChild`. Either leaves `sleep 30` running and this red in about a
/// second.
#[test]
fn a_panicking_on_started_hook_leaves_no_live_child() {
    let master = PtyMaster::open(WinSize::new(80, 24)).expect("a pty");
    let seen = Arc::new(AtomicU64::new(0));

    let hook = {
        let seen = Arc::clone(&seen);
        move |pid: i32| {
            // Recorded *before* the panic, so the test can name the process it is asserting about
            // — which is the whole difficulty: after the unwind there is no handle left to ask.
            seen.store(pid as u64, Ordering::SeqCst);
            panic!("the owner's `started` hook failed");
        }
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        spawn_pty(
            witness(),
            &mut sh("sleep 30"),
            &master,
            StdinPlan::TerminalSlave,
            Some(&hook),
        )
    }));
    assert!(outcome.is_err(), "the hook's panic must reach the caller");

    let pid = seen.load(Ordering::SeqCst) as i32;
    assert!(pid > 0, "the hook ran and saw a pid");
    assert!(
        until(|| !alive(pid)),
        "pid {pid} survived the unwind. Nothing holds its handle, so nothing will ever reap it: \
         this is the untracked live process §9's criterion 3 forbids, arrived at by a panic."
    );
}

/// **The same hole, reached by returning instead of by panicking.**
///
/// `spawn_pty` hands back a `PtyChild` and `PtyHost::adopt` takes it, and between those two the
/// caller is free to do anything — including give up. `PtyHost` has had a `Drop` net for a while
/// and it covers only what has already been adopted, so a caller that dropped the handle in the gap
/// leaked exactly as the panicking one did, quietly and without an unwind to notice.
///
/// **Mutation:** delete `impl Drop for PtyChild`.
#[test]
fn a_child_dropped_before_adoption_is_killed_and_reaped() {
    let master = PtyMaster::open(WinSize::new(80, 24)).expect("a pty");
    let child = spawn_pty(
        witness(),
        &mut sh("sleep 30"),
        &master,
        StdinPlan::TerminalSlave,
        None,
    )
    .expect("the child spawns");
    let pid = child.pid();
    assert!(alive(pid), "it really is running before the drop");
    drop(child);
    assert!(
        until(|| !alive(pid)),
        "pid {pid} outlived the handle that was the only way to reap it"
    );
}

// ---------------------------------------------------------------------------------------------
// The cast records what the node received
// ---------------------------------------------------------------------------------------------

/// **`write_input` records the bytes it writes, or it writes nothing.**
///
/// It used to record `String::from_utf8_lossy(bytes)` and then write the original slice, so `0xff`
/// reached the node and U+FFFD reached the cast. The `i` records are the *only* evidence of what
/// was typed — C1's mouse-through leaves no `o` trace at all — so a recording that differs from
/// what happened is worse than no recording, because nothing downstream can tell.
///
/// asciicast's `data` is a JSON string and has no byte-exact spelling for a byte that is not part
/// of valid UTF-8, so the choice is a private encoding nothing else reads or a refusal. It refuses,
/// **before** the write, which is what makes the two halves below assertable as one property: what
/// the node received and what the cast says are the same on every path, including this one, where
/// both are nothing.
///
/// The multibyte control leg matters as much as the refusal: without it a `write_input` that
/// refused everything non-ASCII would pass.
///
/// **Mutation:** restore `String::from_utf8_lossy(bytes)` and the unconditional
/// `self.master.write_all(bytes)`. The refusal stops being one, `0xff` reaches the slave ahead of
/// the sentinel, and the cast grows an `i` record saying U+FFFD.
#[test]
fn input_the_cast_cannot_carry_is_refused_before_the_node_receives_it() {
    let mut lb = Loopback::new("pty-input-bytes", WinSize::new(80, 24));
    let slave = lb.slave.take().expect("attached");
    nonblocking(slave.as_raw_fd());
    let mut slave = slave;
    let lease = lb
        .host
        .lease_writer(crate::serve::ConnId(1))
        .expect("a fresh host has no writer");

    // Control: a multibyte keystroke goes through untouched, so the refusal below is about the
    // bytes and not about "anything past ASCII".
    lb.host.write_input(&lease, "é\r".as_bytes()).unwrap();

    // The defect's own input. Refused, by kind, with a sentence that says why.
    let refused = lb
        .host
        .write_input(&lease, &[0xff])
        .expect_err("a lone 0xff has no faithful spelling in a JSON string");
    assert_eq!(refused.kind(), io::ErrorKind::InvalidData, "{refused}");
    assert!(
        refused.to_string().contains("not valid UTF-8"),
        "the refusal must say what it refused: {refused}"
    );

    // A sentinel behind it. If `0xff` had been written, it would be in the stream *before* this —
    // so reading the slave and finding the sentinel with no `0xff` ahead of it is a positive
    // statement about what the node received, not an inference from an absence.
    lb.host.write_input(&lease, "Z\r".as_bytes()).unwrap();

    let mut got: Vec<u8> = Vec::new();
    assert!(
        until(|| {
            let mut buf = [0u8; 256];
            match slave.read(&mut buf) {
                Ok(0) => {}
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(_) => {}
            }
            got.contains(&b'Z')
        }),
        "the sentinel never reached the node: {got:?}"
    );
    assert!(
        !got.contains(&0xff),
        "the node received a byte the cast does not record: {got:?}"
    );
    assert!(
        got.starts_with("é".as_bytes()),
        "the control keystroke must have arrived byte-for-byte: {got:?}"
    );

    drop(slave);
    lb.hang_up();
    lb.host.shutdown().unwrap();

    // And the record agrees, in both directions: the two accepted keystrokes are there verbatim,
    // and the refused one left nothing behind — no `i`, and no U+FFFD anywhere.
    let (_, records) = read_cast(&lb.cast);
    let typed: Vec<&str> = records
        .iter()
        .filter(|(_, c, _)| c == "i")
        .map(|(_, _, d)| d.as_str())
        .collect();
    assert_eq!(
        typed,
        vec!["é\r", "Z\r"],
        "the cast must be exactly what the node received: {records:#?}"
    );
    assert!(
        !records
            .iter()
            .any(|(_, c, d)| c == "i" && d.contains('\u{fffd}')),
        "a substitute character in an `i` record is the defect itself: {records:#?}"
    );
}

/// Opaque pane input is byte-exact even when asciicast cannot represent it. The authoritative
/// binary stream persists only content-independent length and ordering evidence, while the
/// compatibility cast deliberately has no fabricated `i` record for bytes it cannot spell.
#[test]
fn opaque_input_reaches_the_slave_exactly_without_a_legacy_i_record() {
    let mut lb = Loopback::new("pty-opaque-input-bytes", WinSize::new(80, 24));
    let slave = lb.slave.take().expect("attached");
    nonblocking(slave.as_raw_fd());
    let mut slave = slave;
    let lease = lb.host.lease_writer(ConnId(91)).unwrap();
    let opaque = [0x00, 0x80, 0xff, b'\n'];

    lb.host
        .write_opaque_input(&lease, &opaque)
        .expect("the binary stream can evidence every byte");

    let mut got = Vec::new();
    assert!(
        until(|| {
            let mut buf = [0_u8; 32];
            match slave.read(&mut buf) {
                Ok(n) if n > 0 => got.extend_from_slice(&buf[..n]),
                _ => {}
            }
            got.ends_with(b"\n")
        }),
        "opaque input did not reach the slave: {got:?}"
    );
    assert_eq!(got, opaque, "the master write must be byte-exact");

    drop(slave);
    lb.hang_up();
    lb.host.shutdown().unwrap();

    let (_, cast) = read_cast(&lb.cast);
    assert!(
        cast.iter().all(|(_, code, _)| code != "i"),
        "opaque input must not fabricate a lossy legacy record: {cast:?}"
    );
    let recovery = read_stream(&lb.cast);
    let evidence = recovery
        .records
        .iter()
        .find_map(|record| match &record.kind {
            super::stream::RecordKind::InputEvidence { byte_len, .. } => Some(*byte_len),
            _ => None,
        })
        .expect("opaque input has durable evidence");
    assert_eq!(evidence, opaque.len() as u32);
    // The raw bytes may legitimately reappear later as terminal output (for example through echo).
    // Privacy is therefore a property of InputEvidence's sequence/length-only representation,
    // asserted in the stream codec tests, rather than a whole-file absence claim.
}

/// Mutation: treat aggregate output-capacity exhaustion as a fatal recorder failure and discard
/// the writer. The next opaque key would then be refused even though its small evidence record and
/// the terminal End still fit inside the bounded stream reserve.
#[test]
fn output_quota_exhaustion_does_not_disable_the_next_opaque_key() {
    let mut lb = Loopback::new("pty-output-quota-keeps-input", WinSize::new(80, 24));
    let child = spawn_pty(
        witness(),
        &mut sh("sleep 30"),
        lb.host.master(),
        StdinPlan::TerminalSlave,
        None,
    )
    .unwrap();
    lb.host.adopt(child);
    let mut slave = lb.slave.take().expect("attached");
    lb.host.limit_durable_output_for_test(128 * 1024);
    let output = vec![b'Q'; 256 * 1024];
    slave
        .write_all(&output)
        .expect("the real PTY reader drains output past its durable output budget");
    assert!(
        until(|| lb.host.bytes_read() >= output.len() as u64),
        "the host did not finish recording output through the quota boundary"
    );

    nonblocking(slave.as_raw_fd());
    let lease = lb.host.lease_writer(ConnId(94)).unwrap();
    let opaque = [0xff, 0x80, 0x00, b'K', b'\n'];
    lb.host
        .write_opaque_input(&lease, &opaque)
        .expect("output capacity must not permanently disable opaque input evidence");

    let mut got = Vec::new();
    assert!(until(|| {
        let mut buf = [0_u8; 32];
        match slave.read(&mut buf) {
            Ok(n) if n > 0 => got.extend_from_slice(&buf[..n]),
            _ => {}
        }
        got.windows(opaque.len()).any(|window| window == opaque)
    }));

    drop(slave);
    lb.hang_up();
    lb.host.shutdown().unwrap();
    let recovery = read_stream(&lb.cast);
    assert!(recovery.records.iter().any(|record| {
        matches!(
            record.kind,
            super::stream::RecordKind::InputEvidence { byte_len, .. }
                if byte_len == opaque.len() as u32
        )
    }));
    assert!(matches!(
        recovery.records.last().map(|record| &record.kind),
        Some(super::stream::RecordKind::End(outcome)) if !outcome.stream_complete
    ));
    assert!(
        lb.host.completed_replay_charge().is_err(),
        "a stream whose output was truncated at its budget must never claim complete replay"
    );
}

/// Mutation: report a failed master delivery only through the host-local completion error while
/// sealing the authoritative End as stream-complete. Recovery would then contradict replay truth.
#[test]
fn input_evidence_followed_by_master_delivery_failure_seals_incomplete_end() {
    let mut lb = Loopback::new("pty-input-delivery-failure-end", WinSize::new(80, 24));
    let child = spawn_pty(
        witness(),
        &mut sh("sleep 30"),
        lb.host.master(),
        StdinPlan::TerminalSlave,
        None,
    )
    .unwrap();
    lb.host.adopt(child);
    let lease = lb.host.lease_writer(ConnId(95)).unwrap();
    lb.host
        .fail_next_master_input("injected master delivery failure after evidence");

    assert!(
        lb.host
            .write_opaque_input(&lease, b"recorded first")
            .is_err(),
        "the injected post-evidence master write must fail"
    );
    lb.hang_up();
    lb.host.shutdown().unwrap();

    let recovery = read_stream(&lb.cast);
    assert!(recovery.records.iter().any(|record| {
        matches!(
            record.kind,
            super::stream::RecordKind::InputEvidence { byte_len, .. }
                if byte_len == b"recorded first".len() as u32
        )
    }));
    assert!(matches!(
        recovery.records.last().map(|record| &record.kind),
        Some(super::stream::RecordKind::End(outcome)) if !outcome.stream_complete
    ));
}

#[test]
fn opaque_evidence_failure_refuses_master_delivery_and_completion() {
    let mut lb = Loopback::new("pty-opaque-input-evidence-failure", WinSize::new(80, 24));
    let slave = lb.slave.take().expect("attached");
    nonblocking(slave.as_raw_fd());
    let mut slave = slave;
    let lease = lb.host.lease_writer(ConnId(92)).unwrap();
    lb.host
        .fail_next_durable_append("injected input evidence sync failure");

    let refused = lb
        .host
        .write_opaque_input(&lease, &[0xff, 0x80, 0x00])
        .expect_err("opaque bytes without durable evidence must be refused");
    assert!(refused.to_string().contains("evidence sync failure"));

    // A compatibility write remains available and is a positive sentinel proving that the
    // nonblocking slave was observed after the refused delivery would have occurred.
    lb.host.write_input(&lease, b"Z\r").unwrap();
    let mut got = Vec::new();
    assert!(until(|| {
        let mut buf = [0_u8; 32];
        match slave.read(&mut buf) {
            Ok(n) if n > 0 => got.extend_from_slice(&buf[..n]),
            _ => {}
        }
        got.contains(&b'Z')
    }));
    assert_eq!(
        got.iter().filter(|byte| **byte == 0xff).count(),
        0,
        "{got:?}"
    );
    assert_eq!(
        got.iter().filter(|byte| **byte == 0x80).count(),
        0,
        "{got:?}"
    );

    drop(slave);
    lb.hang_up();
    lb.host.shutdown().unwrap();
    let error = lb.host.completed_replay_charge().unwrap_err();
    assert!(
        error.to_string().contains("evidence sync failure"),
        "{error}"
    );
    assert!(
        read_stream(&lb.cast)
            .records
            .iter()
            .all(|record| !matches!(record.kind, super::stream::RecordKind::InputEvidence { .. })),
        "the failed append must not advance the input cursor"
    );
}

#[test]
fn authoritative_stream_orders_output_resize_input_and_typed_end_densely() {
    let dir = marion_testsupport::scratch("pty-authoritative-dense-order");
    let cast = dir.join("pty.cast");
    let master = PtyMaster::open(WinSize::new(80, 24)).unwrap();
    let host = PtyHost::start(
        AgentId("authoritative-dense-order".into()),
        master,
        &cast,
        WinSize::new(80, 24),
        "xterm-256color",
        Instant::now(),
    )
    .unwrap();
    let child = spawn_pty(
        witness(),
        &mut sh("printf before; IFS= read -r line; printf after"),
        host.master(),
        StdinPlan::TerminalSlave,
        None,
    )
    .unwrap();
    host.adopt(child);
    assert!(until(|| host.bytes_read() >= b"before".len() as u64));
    host.resize(WinSize::new(100, 31)).unwrap();
    let lease = host.lease_writer(ConnId(93)).unwrap();
    host.write_opaque_input(&lease, b"go\r").unwrap();
    assert!(until(|| host.poll_exited_unreaped().unwrap()));
    let status = host
        .shutdown()
        .unwrap()
        .expect("the adopted child has a status");
    assert_eq!(status.code(), Some(0), "{status:?}");

    let stream = read_stream(&cast);
    assert_eq!(
        stream
            .records
            .iter()
            .map(|record| record.record_seq)
            .collect::<Vec<_>>(),
        (1..=stream.records.len() as u64).collect::<Vec<_>>()
    );
    let output = stream
        .records
        .iter()
        .position(|record| matches!(&record.kind, super::stream::RecordKind::Output(bytes) if bytes.starts_with(b"before")))
        .expect("the initial raw output is durable");
    let resize = stream
        .records
        .iter()
        .position(|record| {
            matches!(
                record.kind,
                super::stream::RecordKind::Resize {
                    rows: 31,
                    cols: 100
                }
            )
        })
        .expect("the geometry boundary is durable");
    let input = stream
        .records
        .iter()
        .position(|record| {
            matches!(
                record.kind,
                super::stream::RecordKind::InputEvidence { byte_len: 3, .. }
            )
        })
        .expect("the input boundary is durable");
    let end = stream.records.len() - 1;
    assert!(
        output < resize && resize < input && input < end,
        "{:?}",
        stream.records
    );
    let super::stream::RecordKind::End(outcome) = stream.records[end].kind else {
        panic!("the final authoritative record is a typed End")
    };
    assert_eq!(outcome.exit_code, Some(0));
    assert_eq!(outcome.signal, None);
    assert!(!outcome.timed_out);
    assert_eq!(outcome.reader, super::stream::ReaderDisposition::CleanEof);
    assert!(outcome.cast_complete);
    assert!(outcome.replay_eligible());
    assert!(host.completed_replay_charge().is_ok());
}

/// Put `fd` in non-blocking mode, so a test can poll the slave without risking a hang whose red
/// state is "the suite stopped".
fn nonblocking(fd: RawFd) {
    // SAFETY: `fd` is open; `F_GETFL` reads and `F_SETFL` writes only the descriptor's flags.
    let flags = unsafe { fcntl(fd, F_GETFL) };
    assert!(flags >= 0, "F_GETFL: {}", io::Error::last_os_error());
    // SAFETY: as above.
    let rc = unsafe { fcntl(fd, F_SETFL, flags | sys::O_NONBLOCK) };
    assert_eq!(rc, 0, "F_SETFL: {}", io::Error::last_os_error());
}

/// **`TIOCSCTTY` is load-bearing on macOS too, and the child here is deliberately not a shell.**
///
/// This is the test the repo spent three doc comments and a §5.3 passage saying was impossible. The
/// claim was that on Darwin, with the slave on fd 0, `setsid()` **alone** already makes the pty the
/// controlling terminal — so deleting the ioctl was an unobservable "platform fact" in this
/// topology, and only `a_piped_stdin_node_still_gets_the_pty_as_its_controlling_terminal` could
/// witness it. Both halves are false, and the second one mattered more than the first.
///
/// Measured directly (`spikes/s19/ctty_probe.c`, `tests/fixtures/s19/README.md`; Darwin 25.5.0,
/// xnu-12377.121.6~2, arm64), nine cells over {slave on fd 0, pipe on fd 0} x {bare, `setsid`,
/// `setsid` + ioctl} x {child opens the slave, parent opens it}: **`setsid()` alone leaves
/// `tcgetsid(master)` at `ENOTTY` in every cell.** The ioctl is what claims the terminal, on both
/// topologies. macOS does not differ from Linux here; the original reading had it backwards.
///
/// # Why the existing tests did not catch that, which is the actual defect
///
/// `the_child_is_a_session_leader_with_the_master_as_its_controlling_terminal` asserts exactly this
/// `tcgetsid` and **survives the deletion**, so it looked like confirmation of the platform claim.
/// It is not: its child is `/bin/sh`, and macOS's `sh` claims the controlling terminal *itself*
/// when it starts as a session leader without one and its stdin is a tty. Swapping that child for
/// `/bin/sleep` under the deletion turns `tcgetsid` from the child's pid straight to `-1`/`ENOTTY`,
/// which is how this was found. Every pty test in this file drives its child through `sh`, so every
/// one of them inherited the confound — the assertion was there, and the shell was quietly
/// satisfying it.
///
/// So the child here is **`/bin/sleep`, execed directly, with no shell anywhere in the topology**.
/// That is the whole point of the test and the one thing about it that must not be "tidied".
///
/// **Mutation:** delete the `ioctl(ctty_fd, TIOCSCTTY, 0)` from `spawn_pty`'s `pre_exec`. This goes
/// red in about a second; nothing else in the suite does.
#[test]
fn tiocsctty_and_not_setsid_is_what_claims_the_terminal() {
    let master = PtyMaster::open(WinSize::new(80, 24)).expect("a pty");
    let master_fd = master.as_raw();
    let mut command = Command::new("/bin/sleep");
    command.arg("5");
    let child = spawn_pty(
        witness(),
        &mut command,
        &master,
        StdinPlan::TerminalSlave,
        None,
    )
    .expect("the child spawns");
    let pid = child.pid();

    assert!(
        until(|| (unsafe { tcgetsid(master_fd) }) == pid),
        "the pty is nobody's controlling terminal: tcgetsid says {} (errno {:?}). With no shell in \
         the child to claim it, `setsid()` alone does not — only the explicit TIOCSCTTY does, on \
         this platform as much as on Linux.",
        unsafe { tcgetsid(master_fd) },
        io::Error::last_os_error().raw_os_error()
    );

    // `PtyChild::drop` kills and reaps; asserted rather than assumed, since this test never builds
    // a `PtyHost` and so has none of the usual teardown.
    drop(child);
    assert!(until(|| !alive(pid)), "pid {pid} outlived its handle");
}
