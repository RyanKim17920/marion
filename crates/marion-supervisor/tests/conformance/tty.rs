//! A pane under a pty, read back through `marion-term` — the P-tui probe's terminal. The pty is
//! marion's own (`pty::PtyMaster`, `pty::spawn_pty`, gated on the row's display-plane witness), so
//! the probe sees what a marion-hosted pane sees.

use std::os::fd::RawFd;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use marion_harness::invocation::Invocation;
use marion_harness::{ControlTransport, PtyWitness};
use marion_supervisor::pty::{PtyChild, PtyMaster, WinSize, spawn_pty, stdin_plan};
use serde_json::json;

use crate::report::Log;

#[repr(C)]
struct PollFd {
    fd: RawFd,
    events: i16,
    revents: i16,
}
unsafe extern "C" {
    fn poll(fds: *mut PollFd, nfds: u32, timeout: i32) -> i32;
}
const POLLIN: i16 = 1;

struct State {
    bytes: Vec<u8>,
    term: marion_term::Term,
    last: Instant,
    eof: bool,
}

pub struct Tty {
    master: Arc<PtyMaster>,
    child: Mutex<PtyChild>,
    st: Arc<(Mutex<State>, Condvar)>,
    stop: Arc<AtomicBool>,
    log: Arc<Log>,
}

pub fn has(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

impl Tty {
    pub fn spawn(
        witness: PtyWitness,
        control: ControlTransport,
        inv: &Invocation,
        log: Arc<Log>,
    ) -> std::io::Result<Self> {
        let (cols, rows) = (120u16, 40u16);
        let master = Arc::new(PtyMaster::open(WinSize::new(cols, rows))?);
        let mut cmd = Command::new(&inv.program);
        cmd.args(&inv.args)
            .envs(inv.env.iter().cloned())
            .env(
                "MARION_DUMMY_KEY",
                marion_supervisor::run::PLACEHOLDER_API_KEY,
            )
            .env("TERM", "xterm-256color")
            .current_dir(&inv.cwd);
        let child = spawn_pty(witness, &mut cmd, &master, stdin_plan(control), None)?;
        log.w(
            "note",
            &json!({"spawned in a pty": inv.program, "args": inv.args, "pid": child.pid()}),
        );
        let st = Arc::new((
            Mutex::new(State {
                bytes: Vec::new(),
                term: marion_term::Term::new(marion_term::Size::new(cols.into(), rows.into())),
                last: Instant::now(),
                eof: false,
            }),
            Condvar::new(),
        ));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (master, st, stop, log) = (
                Arc::clone(&master),
                Arc::clone(&st),
                Arc::clone(&stop),
                Arc::clone(&log),
            );
            std::thread::spawn(move || {
                let mut buf = [0u8; 16384];
                loop {
                    let mut p = PollFd {
                        fd: master.as_raw(),
                        events: POLLIN,
                        revents: 0,
                    };
                    // SAFETY: one live pollfd, its count passed exactly.
                    let ready = unsafe { poll(&mut p, 1, 1000) };
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if ready <= 0 {
                        continue;
                    }
                    match master.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            log.w("raw", &json!(String::from_utf8_lossy(&buf[..n])));
                            let (m, cv) = &*st;
                            let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
                            g.bytes.extend_from_slice(&buf[..n]);
                            g.term.advance(&buf[..n]);
                            g.last = Instant::now();
                            cv.notify_all();
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                        Err(_) => break,
                    }
                }
                let (m, cv) = &*st;
                m.lock().unwrap_or_else(|e| e.into_inner()).eof = true;
                cv.notify_all();
            });
        }
        Ok(Self {
            master,
            child: Mutex::new(child),
            st,
            stop,
            log,
        })
    }

    pub fn wait(
        &self,
        bound: Duration,
        mut pred: impl FnMut(&[u8], &marion_term::Term) -> bool,
    ) -> bool {
        let deadline = Instant::now() + bound;
        let (m, cv) = &*self.st;
        let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if pred(&g.bytes, &g.term) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline || g.eof {
                return false;
            }
            g = cv
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Wait for `quiet` of no output (IdleSignal::OutputQuiet's reading), within `bound`.
    pub fn quiet(&self, quiet: Duration, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        let (m, cv) = &*self.st;
        let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let since = g.last.elapsed();
            if since >= quiet {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let step = (quiet - since).min(deadline - now);
            g = cv
                .wait_timeout(g, step)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    pub fn screen(&self) -> Vec<String> {
        let (m, _) = &*self.st;
        let g = m.lock().unwrap_or_else(|e| e.into_inner());
        g.term
            .viewport_lines()
            .into_iter()
            .map(|l| l.trim_end().to_string())
            .collect()
    }

    pub fn write(&self, bytes: &[u8]) {
        self.log.w("c2s", &json!(String::from_utf8_lossy(bytes)));
        let _ = self.master.write_all(bytes);
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        // Kill and reap first, then stop the reader: the master read is what drains an exiting
        // session leader's output queue (tasks/lessons.md).
        let _ = self
            .child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .kill_and_reap();
        self.stop.store(true, Ordering::Relaxed);
    }
}
