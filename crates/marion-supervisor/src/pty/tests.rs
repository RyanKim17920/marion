//! Tests for the pty host.
//!
//! Every one names the mutation it exists to catch, and none of them can fail only by timing out:
//! a test whose red state is "the suite hung" is a test nobody will run twice. Where a real process
//! is involved the wait is bounded by [`until`] and the failure is an assertion, with `kill(pid, 0)`
//! consulted first so the message says whether the child was alive or the event never came.

use super::*;
use marion_harness::{ControlTransport, ExecutionSurfaces, TypedKind};
use std::time::Duration;

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

/// Poll `cond` for at most five seconds, then answer.
///
/// Five is `handler.rs`'s own bound, and this matches it rather than inventing a second one. It is
/// not a timeout in the "the suite hung" sense: every caller turns the `false` into an assertion
/// whose message names what did not happen, and most of them ask `kill(pid, 0)` first so the
/// message separates "the child died" from "the event never came". A tighter bound was tried at two
/// seconds and produced one failure in roughly twenty full-suite runs, on a `fork`/`exec` under a
/// fully loaded machine — which is a measurement of the laptop, not of the code.
fn until(mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    cond()
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
        let dir = marion_testsupport::scratch(tag);
        let cast = dir.join("pty.cast");
        let master = PtyMaster::open(size).expect("a pty");
        let slave = std::fs::File::from(master.open_slave().expect("the slave opens"));
        let host = PtyHost::start(
            AgentId("node-under-test".into()),
            master,
            &cast,
            size,
            "xterm-256color",
            Instant::now(),
        )
        .expect("the host starts");
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
/// **`TIOCSCTTY` is *not* observable from this topology on macOS, and that is a measurement rather
/// than a gap in the test.** With the slave on fd 0, Darwin 25.5.0 makes the pty the child's
/// controlling terminal on `setsid()` alone: a three-way C probe (no `setsid` → `tcgetsid` is
/// `ENOTTY`; `setsid` only → `tcgetsid == pid`; `setsid` + ioctl → `tcgetsid == pid`) says the
/// ioctl changes nothing here. It is kept because Linux does require it, and it is pinned by
/// `a_piped_stdin_node_still_gets_the_pty_as_its_controlling_terminal`, which uses the one topology
/// where macOS can tell the difference.
#[test]
fn the_child_is_a_session_leader_with_the_master_as_its_controlling_terminal() {
    let dir = marion_testsupport::scratch("pty-sid");
    let master = PtyMaster::open(WinSize::new(80, 24)).expect("a pty");
    let slave_path = master.slave_path().to_path_buf();
    let master_fd = master.as_raw();
    let mut host = PtyHost::start(
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
/// It is also the **only** topology on macOS in which `TIOCSCTTY` is observable at all. Measured on
/// Darwin 25.5.0 with a three-way C probe: with the slave on fd 0, `setsid()` alone already makes
/// the pty the controlling terminal and the ioctl changes nothing; with stdin a pipe,
/// `setsid()` alone leaves `tcgetsid(master)` at `ENOTTY`, and only `ioctl(1, TIOCSCTTY, 0)` claims
/// it. So this test is where the "drop `TIOCSCTTY`" mutation goes red on this platform.
///
/// **Mutation:** delete the `TIOCSCTTY` ioctl; or issue it on fd 0 regardless of the stdin plan,
/// which is the shape §5.3 specifies.
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
    let mut host = PtyHost::start(
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
    let mut host = PtyHost::start(
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
    lb.host.write_input(b"/help\r").unwrap();
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
}

/// **A resize record precedes the output it explains**, which inverts `s2/ptyhost.py` on purpose.
///
/// The interleaving is *forced*, not hoped for: the yield hook runs between the `r` record and
/// `TIOCSWINSZ`, and it makes the child speak. Under the shipped order the `r` is already on disk
/// when that happens; under the mutation the ioctl has already run and the hook's output lands
/// first. A test that depended on winning a race would be a test that passes by luck.
///
/// **Mutation:** swap the body of `PtyHost::resize` to ioctl-then-record.
#[test]
fn a_resize_record_precedes_the_output_it_explains() {
    let mut lb = Loopback::new("pty-resize-order", WinSize::new(120, 40));
    // The hook runs on the caller's thread, between the two steps. It writes from the slave side
    // and waits until the host has recorded it, so the two records are ordered by observation
    // rather than by scheduling luck.
    let slave = lb.slave.take().expect("attached");
    let bytes_before = lb.host.bytes_read();
    let counter = lb.host.bytes_counter();
    let seen = Arc::new(AtomicU64::new(0));
    {
        let seen = Arc::clone(&seen);
        let slave = Mutex::new(slave);
        lb.host.set_resize_hook(Box::new(move || {
            let _ = slave
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .write_all(b"AFTER");
            // **Wait for the recorder before returning.** Without this the hook only *starts* the
            // output, and whether the `o` record lands before or after the `r` is down to thread
            // scheduling — which is the race this test exists to remove, not to run.
            assert!(
                until(|| counter.load(Ordering::SeqCst) >= bytes_before + 5),
                "the hook's bytes never reached the recorder"
            );
            seen.store(1, Ordering::SeqCst);
        }));
    }
    lb.host.resize(WinSize::new(100, 24)).unwrap();
    assert_eq!(seen.load(Ordering::SeqCst), 1, "the hook ran");
    lb.host.clear_resize_hook();
    lb.hang_up();
    lb.host.shutdown().unwrap();

    let (_, records) = read_cast(&lb.cast);
    let r_at = records
        .iter()
        .position(|(_, c, _)| c == "r")
        .expect("a resize record");
    let o_at = records
        .iter()
        .position(|(_, c, d)| c == "o" && d.contains("AFTER"))
        .expect("the output the hook produced");
    assert!(
        r_at < o_at,
        "output at a size nothing recorded is unreplayable: r at {r_at}, output at {o_at} in \
         {records:?}"
    );
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
    let mut host = PtyHost::start(
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
    let mut host = PtyHost::start(
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

/// A host dropped without `shutdown` still leaves no live child. §9's M2 criterion forbids an
/// untracked live process, and a leaked pty child is exactly one.
#[test]
fn dropping_a_host_leaves_no_child_behind() {
    let dir = marion_testsupport::scratch("pty-drop");
    let master = PtyMaster::open(WinSize::new(80, 24)).unwrap();
    let mut host = PtyHost::start(
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
