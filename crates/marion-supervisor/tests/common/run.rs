//! **The `marion run` under test, and the cleanup that outlives a failing assertion.**
//!
//! Shared by `client_run.rs` and `node_attach.rs`, which each used to carry a copy.

use std::path::Path;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marion_provider::TurnGate;
use marion_testsupport::sweep;

use super::client::BOUND;

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// A `marion run` in flight, plus everything that must be undone whether it finishes or not.
///
/// A guard rather than trailing statements, for `marion_testsupport::Scratch`'s reason: a failing
/// assertion unwinds straight past cleanup, so the runs that leak are exactly the runs that failed.
/// **The gate is released first** on drop — a held provider turn is a node parked for ever, and
/// tearing the run down without releasing would leave the provider's connection thread blocked on a
/// condvar for the life of the test binary.
pub struct Run {
    child: Option<Child>,
    gate: Arc<TurnGate>,
    needle: String,
}

impl Run {
    pub fn pid(&self) -> i32 {
        self.child.as_ref().expect("still held").id() as i32
    }

    /// SIGKILL the client and wait for it to be gone. **Uncatchable, and that is the point**: §7.3.1
    /// distinguishes a client that *said* it was leaving from one that vanished, and a crash is the
    /// second. Waited on rather than merely signalled, so every assertion after this is about a
    /// process that has actually stopped.
    pub fn kill_client(&mut self) {
        let mut c = self.child.take().expect("killed once");
        // SAFETY: `kill` on the pid of a child this process spawned and has not reaped.
        unsafe { kill(c.id() as i32, 9) };
        let _ = c.wait();
    }

    /// Release the held turn and wait for the run to finish on its own, within [`BOUND`].
    pub fn wait(&mut self) -> std::process::ExitStatus {
        self.gate.release();
        let mut c = self.child.take().expect("waited once");
        let deadline = Instant::now() + BOUND;
        loop {
            match c.try_wait().expect("try_wait") {
                Some(s) => return s,
                None if Instant::now() >= deadline => {
                    let _ = c.kill();
                    let _ = c.wait();
                    panic!("`marion run` did not finish within {BOUND:?}");
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.gate.release();
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        // **Waited on, not merely signalled.** See `marion_testsupport::sweep`: the `Scratch` guard
        // removes this tree the moment this returns, and a process that has been `SIGKILL`ed but
        // has not died yet can still land the write it was already inside — which is how a run
        // that cleaned up after itself left one `contracts/<task>.json` behind.
        sweep(&self.needle);
    }
}

/// `marion run claude --canned` against `base_url`, prompted to delegate the marker-file task to a
/// child. `root_marker` keys the root's half of the suite's canned script; `blocked_secs` is the
/// root's `--timeout`, §9's per-episode `Blocked`-only budget rather than a wall clock.
pub fn start_run(
    dir: &Path,
    repo: &Path,
    state: &Path,
    base_url: &str,
    gate: &Arc<TurnGate>,
    root_marker: &str,
    blocked_secs: &str,
) -> Run {
    let child = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args([
            "run",
            "claude",
            "--prompt",
            &format!("{root_marker}: delegate the marker-file task to a child."),
            "--repo",
            &repo.to_string_lossy(),
            "--state-dir",
            &state.to_string_lossy(),
            "--base-url",
            base_url,
            "--canned",
            "--timeout",
            blocked_secs,
        ])
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("marion run starts");
    Run {
        child: Some(child),
        gate: Arc::clone(gate),
        needle: dir.display().to_string(),
    }
}
