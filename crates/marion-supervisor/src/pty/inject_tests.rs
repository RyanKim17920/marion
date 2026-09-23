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
