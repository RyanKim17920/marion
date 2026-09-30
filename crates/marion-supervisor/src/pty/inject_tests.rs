//! Tests for what the host knows about its terminal's input side, and for marion typing into it.
//!
//! Every host here has **no child**: the test holds the slave, writes the node's output into it by
//! hand and reads what the master delivered straight back off it, in raw mode, so the bytes are
//! exactly what a harness would have read. Waits are on the host's own completion counters, never
//! on a sleep.

use super::*;
use marion_testsupport::until;
use std::io::Read;

/// A child-less host whose slave is in raw mode: no echo, no line discipline, so what the master
/// wrote is what a read of the slave returns.
struct RawLoopback {
    _dir: marion_testsupport::Scratch,
    cast: PathBuf,
    host: PtyHost,
    slave: std::fs::File,
}

impl RawLoopback {
    fn new(tag: &str) -> Self {
        let dir = marion_testsupport::scratch(tag);
        let cast = dir.join("pty.cast");
        let size = WinSize::new(80, 24);
        let master = PtyMaster::open(size).expect("a pty");
        let slave = std::fs::File::from(master.open_slave().expect("the slave opens"));
        let mut termios = rustix::termios::tcgetattr(&slave).expect("the slave's termios");
        termios.make_raw();
        rustix::termios::tcsetattr(&slave, rustix::termios::OptionalActions::Now, &termios)
            .expect("the slave goes raw");
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
            slave,
        }
    }

    /// Write bytes as the node would, and wait until the host has recorded them.
    fn node_writes(&mut self, bytes: &[u8]) {
        let before = self.host.bytes_read();
        self.slave
            .write_all(bytes)
            .expect("the slave accepts a write");
        assert!(
            until(|| self.host.bytes_read() >= before + bytes.len() as u64),
            "the host never read the {} bytes the slave wrote",
            bytes.len()
        );
    }
}

/// **DECSET 2004 is read off the node's own output**, across a read boundary, with a combined
/// parameter list, and reset by `RIS`. Mutation caught: a paste written to a terminal that never
/// asked for bracketed paste — whose embedded newline would submit half a message.
#[test]
fn bracketed_paste_mode_follows_the_nodes_own_output() {
    let mut lb = RawLoopback::new("pty-2004-scan");
    assert!(!lb.host.bracketed_paste(), "off until the node asks for it");
    lb.node_writes(b"hello \x1b[?20");
    assert!(
        !lb.host.bracketed_paste(),
        "half a sequence is not a request"
    );
    lb.node_writes(b"04h world");
    assert!(
        lb.host.bracketed_paste(),
        "split across two reads, still one request"
    );
    lb.node_writes(b"\x1b[?1004;2004l");
    assert!(!lb.host.bracketed_paste(), "a combined reset turns it off");
    lb.node_writes(b"\x1b[?1049;2004;1004h");
    assert!(lb.host.bracketed_paste(), "a combined set turns it on");
    lb.node_writes(b"\x1b[?2004$p\x1b[2004l\x1b[?12004l");
    assert!(
        lb.host.bracketed_paste(),
        "a mode query, a non-private reset and another number are not a reset"
    );
    lb.node_writes(b"\x1bc");
    assert!(!lb.host.bracketed_paste(), "RIS resets every mode");
}

impl RawLoopback {
    /// Everything the master has delivered to the slave so far and within `quiet` after it.
    fn drain_slave(&mut self, want: usize) -> Vec<u8> {
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        while got.len() < want {
            let n = self.slave.read(&mut buf).expect("the slave reads");
            assert!(
                n > 0,
                "the slave hung up with {} of {want} bytes",
                got.len()
            );
            got.extend_from_slice(&buf[..n]);
        }
        got
    }

    fn records(&self) -> Vec<(f64, String, String)> {
        let text = std::fs::read_to_string(&self.cast).expect("pty.cast exists");
        text.lines()
            .skip(1)
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).expect("an [interval, code, data] record"))
            .collect()
    }
}

fn paste<'a>(label: &'a str, body: &'a [u8]) -> Injection<'a> {
    Injection {
        label,
        body,
        submit: b"\r",
        submit_delay: Duration::from_millis(50),
    }
}

/// **The operator's typing is what the host remembers**: when it last typed, and whether its last
/// keystroke submitted. Focus reports are the operator's terminal talking, not the operator, and
/// move neither.
#[test]
fn operator_input_is_tracked_from_the_write_path() {
    let lb = RawLoopback::new("pty-operator-input");
    let fresh = lb.host.input_state();
    assert_eq!(fresh.last_operator_input, None, "nobody has typed");
    assert!(fresh.composer_empty, "and nothing is half-typed");

    let lease = lb.host.lease_writer(ConnId(7)).expect("the write half");
    lb.host.write_opaque_input(&lease, b"hel").unwrap();
    let typing = lb.host.input_state();
    let typed_at = typing.last_operator_input.expect("typing is recorded");
    assert!(
        !typing.composer_empty,
        "a half-typed line is in the composer"
    );

    lb.host.write_opaque_input(&lease, b"\x1b[I").unwrap();
    lb.host.write_opaque_input(&lease, b"\x1b[O").unwrap();
    let focused = lb.host.input_state();
    assert_eq!(
        focused.last_operator_input,
        Some(typed_at),
        "a focus report is not typing"
    );
    assert!(!focused.composer_empty);

    lb.host.write_opaque_input(&lease, b"lo\r").unwrap();
    let submitted = lb.host.input_state();
    assert!(
        submitted.composer_empty,
        "the opaque path's Enter submitted the line"
    );
    assert!(submitted.last_operator_input.unwrap() >= typed_at);

    lb.host.write_opaque_input(&lease, b"x\x1b[13u").unwrap();
    assert!(
        lb.host.input_state().composer_empty,
        "a kitty-keyboard Enter submits too"
    );
    lb.host.write_opaque_input(&lease, b"\x1b[A").unwrap();
    assert!(
        !lb.host.input_state().composer_empty,
        "an arrow can recall history into the composer, so it is presumed to have"
    );
}

/// **A paste is written as given, then the submit after the delay, and the cast says marion
/// typed it**: an `m` marker naming the injection, then the two `i` records, in that order.
#[test]
fn an_injection_is_recorded_as_marion_and_submitted_after_its_delay() {
    let mut lb = RawLoopback::new("pty-inject");
    let body = b"\x1b[200~hello\nworld\x1b[201~";
    let outcome = lb
        .host
        .inject(&paste("marion: paste m-1", body), &|_| true)
        .expect("the injection is written");
    assert_eq!(outcome, Injected::Written);
    let mut want = body.to_vec();
    want.push(b'\r');
    assert_eq!(
        lb.drain_slave(want.len()),
        want,
        "byte-exact, the submit last"
    );

    let records = lb.records();
    let at = records
        .iter()
        .position(|(_, c, d)| c == "m" && d == "marion: paste m-1")
        .expect("the cast marks marion's injection");
    assert_eq!(records[at + 1].1, "i");
    assert_eq!(records[at + 1].2.as_bytes(), body);
    assert_eq!(
        (records[at + 2].1.as_str(), records[at + 2].2.as_str()),
        ("i", "\r")
    );
    assert!(
        records[at + 2].0 >= 0.05,
        "the submit waited its delay after the paste: {}",
        records[at + 2].0
    );
    assert_eq!(
        lb.host.input_state().last_operator_input,
        None,
        "marion's typing is not the operator's"
    );
}

/// **The guard is asked under the write lock, with the state the operator's writes left**, and a
/// declined injection writes and records nothing: the next byte the slave reads is the operator's.
#[test]
fn a_declined_injection_writes_nothing() {
    let mut lb = RawLoopback::new("pty-inject-declined");
    let lease = lb.host.lease_writer(ConnId(3)).expect("the write half");
    lb.host.write_opaque_input(&lease, b"ab").unwrap();
    assert_eq!(lb.drain_slave(2), b"ab");
    let seen = std::sync::Mutex::new(None);
    let outcome = lb
        .host
        .inject(&paste("marion: paste m-2", b"no"), &|state| {
            *seen.lock().unwrap() = Some(*state);
            state.composer_empty
        })
        .unwrap();
    let state = seen.lock().unwrap().expect("the guard was asked");
    assert!(
        !state.composer_empty,
        "it saw the operator's half-typed line"
    );
    assert_eq!(outcome, Injected::Declined);
    lb.host.write_opaque_input(&lease, b"c").unwrap();
    assert_eq!(
        lb.drain_slave(1),
        b"c",
        "nothing of the declined paste reached the node"
    );
    assert!(
        !lb.records().iter().any(|(_, c, d)| c == "m" || d == "no"),
        "and nothing of it reached the cast"
    );
}

/// **8 KiB — the inbox's cap — arrives byte-exact**, past the pty's own input buffer: the write
/// waits for the node to read rather than truncating or failing.
#[test]
fn an_injection_the_size_of_the_inbox_cap_arrives_byte_exact() {
    let mut lb = RawLoopback::new("pty-inject-8k");
    let text: String = (0..8 * 1024)
        .map(|i| (b'a' + (i % 26) as u8) as char)
        .collect();
    let body = format!("\x1b[200~{text}\x1b[201~");
    let mut reader = lb.slave.try_clone().expect("a second handle on the slave");
    let want = body.len() + 1;
    let read = std::thread::spawn(move || {
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        while got.len() < want {
            let n = reader.read(&mut buf).expect("the slave reads");
            assert!(n > 0, "the slave hung up");
            got.extend_from_slice(&buf[..n]);
        }
        got
    });
    lb.host
        .inject(&paste("marion: paste m-3", body.as_bytes()), &|_| true)
        .unwrap();
    let got = read.join().unwrap();
    assert_eq!(got.len(), want);
    assert_eq!(&got[..body.len()], body.as_bytes());
    assert_eq!(got[body.len()], b'\r');
    let _ = &mut lb;
}

/// Once the pane is closing, marion types nothing more into it.
#[test]
fn an_injection_after_closing_began_is_refused() {
    let lb = RawLoopback::new("pty-inject-closing");
    lb.host.seal_controls();
    let err = lb
        .host
        .inject(&paste("marion: paste m-4", b"late"), &|_| true)
        .expect_err("a closing pane takes no input");
    assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
}
