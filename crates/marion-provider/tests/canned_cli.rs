//! `marion-canned` as a person meets it: asking it for help must not start a server.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// **`--help` explains and exits**; it used to bind the port and park forever, leaving a server a
/// person did not ask for.
#[test]
fn help_prints_usage_and_exits_without_serving() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_marion-canned"))
        .arg("--help")
        // Port 0, so a regression binds an ephemeral port rather than a shared 8099.
        .env("MARION_CANNED_PORT", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("marion-canned runs");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let status = status.expect("marion-canned --help must exit rather than serve");
    assert!(status.success(), "{status:?}");
    let out = child.wait_with_output().expect("output");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("usage: marion-canned"), "{text}");
    assert!(text.contains("MARION_CANNED_PORT"), "{text}");
}
