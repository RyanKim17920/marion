//! Driving a harness process: one [`Proc`] for every surface, and a [`Node`] that speaks the
//! surface the row declares — argv (`LaunchOnly`), Claude Code's stream-json (`Duplex`) or ACP.
//!
//! The surface is **chosen by `duplex::launch_path` over the row's surfaces**, the same function
//! marion's own spawn branches on, so no probe knows which harness it is driving. Every wait is on
//! a condition variable the reader threads signal; nothing here polls a timer.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use marion_harness::acp;
use marion_harness::invocation::Invocation;
use marion_supervisor::duplex::{self, LaunchPath};
use serde_json::{Value, json};

use crate::report::Log;
use crate::target::Launch;

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
pub const SIGINT: i32 = 2;
pub const SIGKILL: i32 = 9;
pub const SIGTERM: i32 = 15;

/// Everything a process has said, and whether it has ended.
#[derive(Debug, Default)]
pub struct Io {
    pub frames: Vec<Value>,
    pub stdout: String,
    pub stderr: String,
    pub out_eof: bool,
    pub exit: Option<ExitStatus>,
    pub exited_at: Option<Instant>,
}

/// Answers a frame the process sent that expects an answer (a server request), or nothing.
pub type Responder = Box<dyn Fn(&Value) -> Option<Value> + Send + Sync>;

pub struct Proc {
    pub pid: i32,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    io: Arc<(Mutex<Io>, Condvar)>,
    log: Arc<Log>,
    /// Every descendant seen while the process ran, for the leak check after it ends.
    seen: Mutex<BTreeMap<i32, String>>,
}

impl Proc {
    pub fn spawn(
        inv: &Invocation,
        piped_stdin: bool,
        log: Arc<Log>,
        responder: Option<Responder>,
    ) -> std::io::Result<Self> {
        let mut cmd = Command::new(&inv.program);
        cmd.args(&inv.args)
            .envs(inv.env.iter().cloned())
            // What marion's canned spawn adds beside the compiled env (`run::launch_only_child`):
            // the placeholder a generated config's `env_key` names.
            .env(
                "MARION_DUMMY_KEY",
                marion_supervisor::run::PLACEHOLDER_API_KEY,
            )
            .current_dir(&inv.cwd)
            .process_group(0)
            .stdin(if piped_stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        let pid = child.id() as i32;
        log.w(
            "note",
            &json!({"spawned": inv.program, "args": inv.args, "pid": pid}),
        );
        let stdin = Arc::new(Mutex::new(child.stdin.take()));
        let io: Arc<(Mutex<Io>, Condvar)> = Arc::default();
        {
            let (io, log, stdin) = (Arc::clone(&io), Arc::clone(&log), Arc::clone(&stdin));
            let out = child.stdout.take().expect("piped");
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines() {
                    let Ok(line) = line else { break };
                    let frame = serde_json::from_str::<Value>(&line).ok();
                    match &frame {
                        Some(v) => log.w("s2c", v),
                        None => log.w("raw", &json!(line)),
                    }
                    if let (Some(v), Some(r)) = (&frame, &responder)
                        && let Some(reply) = r(v)
                    {
                        write_line(&stdin, &log, &reply);
                    }
                    let (m, cv) = &*io;
                    let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
                    g.stdout.push_str(&line);
                    g.stdout.push('\n');
                    if let Some(v) = frame {
                        g.frames.push(v);
                    }
                    cv.notify_all();
                }
                let (m, cv) = &*io;
                m.lock().unwrap_or_else(|e| e.into_inner()).out_eof = true;
                cv.notify_all();
            });
        }
        {
            let (io, log) = (Arc::clone(&io), Arc::clone(&log));
            let mut err = child.stderr.take().expect("piped");
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                while let Ok(n) = err.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = buf.drain(..=nl).collect();
                        let line = String::from_utf8_lossy(&line).trim_end().to_string();
                        log.w("err", &json!(line));
                        let (m, cv) = &*io;
                        let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
                        g.stderr.push_str(&line);
                        g.stderr.push('\n');
                        cv.notify_all();
                    }
                }
            });
        }
        {
            let (io, log) = (Arc::clone(&io), Arc::clone(&log));
            std::thread::spawn(move || {
                let status = child.wait();
                if let Ok(s) = &status {
                    log.w(
                        "note",
                        &json!({"exited": pid, "code": s.code(), "signal": s.signal()}),
                    );
                }
                let (m, cv) = &*io;
                let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
                g.exit = status.ok();
                g.exited_at = Some(Instant::now());
                cv.notify_all();
            });
        }
        Ok(Self {
            pid,
            stdin,
            io,
            log,
            seen: Mutex::default(),
        })
    }

    pub fn send(&self, v: &Value) {
        write_line(&self.stdin, &self.log, v);
    }

    pub fn send_text(&self, line: &str) {
        match serde_json::from_str::<Value>(line) {
            Ok(v) => self.send(&v),
            Err(_) => {
                let mut g = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(s) = g.as_mut() {
                    let _ = writeln!(s, "{line}");
                    let _ = s.flush();
                }
            }
        }
    }

    pub fn close_stdin(&self) {
        self.log.note("stdin closed");
        self.stdin.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    /// Wait until `pred` holds over what the process has said, or the bound passes.
    pub fn wait(&self, bound: Duration, mut pred: impl FnMut(&Io) -> bool) -> bool {
        let deadline = Instant::now() + bound;
        let (m, cv) = &*self.io;
        let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if pred(&g) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            g = cv
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    pub fn wait_exit(&self, bound: Duration) -> Option<ExitStatus> {
        self.wait(bound, |io| io.exit.is_some());
        self.with(|io| io.exit)
    }

    pub fn with<R>(&self, f: impl FnOnce(&Io) -> R) -> R {
        let (m, _) = &*self.io;
        f(&m.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn stdout(&self) -> String {
        self.with(|io| io.stdout.clone())
    }

    pub fn stderr(&self) -> String {
        self.with(|io| io.stderr.clone())
    }

    pub fn signal_group(&self, sig: i32) {
        self.log
            .note(format!("signal {sig} to process group {}", self.pid));
        // SAFETY: a negative pid addresses the group this process leads (`process_group(0)`).
        let _ = unsafe { kill(-self.pid, sig) };
    }

    /// Record every live descendant (by parentage and by process group) for the leak check.
    pub fn snapshot(&self) {
        let rows = ps();
        let mut found = vec![self.pid];
        let mut i = 0;
        while i < found.len() {
            let p = found[i];
            for r in &rows {
                if (r.ppid == p || r.pgid == self.pid) && !found.contains(&r.pid) {
                    found.push(r.pid);
                }
            }
            i += 1;
        }
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        for r in rows.iter().filter(|r| found.contains(&r.pid)) {
            seen.entry(r.pid).or_insert_with(|| r.command.clone());
        }
    }

    /// The processes seen during the run that are still alive — same pid, same command, not a
    /// zombie — plus anything still in the process's group.
    pub fn stragglers(&self) -> Vec<(i32, String)> {
        let seen = self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        ps().into_iter()
            .filter(|r| !r.stat.starts_with('Z'))
            .filter(|r| seen.get(&r.pid) == Some(&r.command) || r.pgid == self.pid)
            .map(|r| (r.pid, r.command))
            .collect()
    }

    /// Kill everything this process started that is still alive.
    pub fn sweep(&self) {
        self.signal_group(SIGKILL);
        for (pid, _) in self.stragglers() {
            marion_testsupport::kill_hard(pid);
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.snapshot();
        self.sweep();
    }
}

fn write_line(stdin: &Mutex<Option<ChildStdin>>, log: &Log, v: &Value) {
    log.w("c2s", v);
    let mut g = stdin.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = g.as_mut() {
        let _ = writeln!(s, "{v}");
        let _ = s.flush();
    }
}

pub struct PsRow {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub stat: String,
    pub command: String,
}

pub fn ps() -> Vec<PsRow> {
    let out = Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid=,stat=,command="])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let pgid = it.next()?.parse().ok()?;
            let stat = it.next()?.to_string();
            let command = it.collect::<Vec<_>>().join(" ");
            Some(PsRow {
                pid,
                ppid,
                pgid,
                stat,
                command,
            })
        })
        .collect()
}

/// Whether to take marion's own readiness gate before the first prompt.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// What marion does: the duplex path waits for the bridge's ready file, ACP for `session/new`.
    Marion,
    /// Write the prompt as soon as the harness can take it — the readiness probe's control.
    None,
}

/// A running node on the surface its row declares.
pub struct Node {
    pub proc: Proc,
    pub path: LaunchPath,
    pub session: Option<String>,
    next_id: u64,
    prompt_ids: Vec<u64>,
}

/// How long a harness has to come up before a probe calls it unresponsive.
pub const BOOT: Duration = Duration::from_secs(60);

/// marion's answer to each server request its drivers answer: a stream-json permission ask is
/// denied (`run::duplex_child`'s zero budget), an ACP permission ask gets the agent's own allow
/// option (`acp_child`), and anything else is refused as unsupported, as marion refuses it.
fn responder(path: LaunchPath) -> Option<Responder> {
    match path {
        LaunchPath::Duplex => Some(Box::new(|v: &Value| {
            if let Some((id, tool)) = duplex::can_use_tool_request(v) {
                let reply = duplex::deny_response(
                    &id,
                    &format!("marion denies `{tool}`: no operator answers a child's ask"),
                );
                return serde_json::from_str(&reply).ok();
            }
            duplex::unanswerable_control_request(v).and_then(|(id, subtype)| {
                serde_json::from_str(&duplex::unsupported_request_response(&id, &subtype)).ok()
            })
        })),
        LaunchPath::Acp => Some(Box::new(|v: &Value| {
            let method = v.get("method")?.as_str()?;
            let id = v.get("id")?.clone();
            if method == "session/request_permission" {
                return Some(
                    match marion_supervisor::acp_child::allow_option(&v["params"]) {
                        Some(opt) => json!({"jsonrpc": "2.0", "id": id,
                        "result": {"outcome": {"outcome": "selected", "optionId": opt}}}),
                        None => json!({"jsonrpc": "2.0", "id": id,
                        "result": {"outcome": {"outcome": "cancelled"}}}),
                    },
                );
            }
            Some(json!({"jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": format!("marion does not serve {method}")}}))
        })),
        LaunchPath::LaunchOnly | LaunchPath::Terminal => None,
    }
}

impl Node {
    /// Launch and start the first turn. On `LaunchOnly` the prompt is already on argv.
    pub fn start(
        path: LaunchPath,
        launch: &Launch,
        prompt: &str,
        log: Arc<Log>,
        gate: Gate,
    ) -> Result<Self, String> {
        let piped = !matches!(path, LaunchPath::LaunchOnly);
        let proc = Proc::spawn(&launch.inv, piped, log, responder(path))
            .map_err(|e| format!("spawn {}: {e}", launch.inv.program))?;
        let mut node = Node {
            proc,
            path,
            session: None,
            next_id: 10,
            prompt_ids: Vec::new(),
        };
        match path {
            LaunchPath::LaunchOnly | LaunchPath::Terminal => {}
            LaunchPath::Duplex => {
                if gate == Gate::Marion
                    && let Some(f) = &launch.ready_file
                    && !duplex::wait_for_ready(f, BOOT)
                {
                    return Err("marion's bridge never wrote its ready file".into());
                }
                let init = "marion-conformance-init";
                node.proc.send_text(&duplex::initialize_request(init));
                if !node.proc.wait(BOOT, |io| {
                    io.frames
                        .iter()
                        .any(|f| duplex::is_control_response_to(f, init))
                        || io.exit.is_some()
                }) {
                    return Err("no answer to the stream-json initialize".into());
                }
                node.prompt(prompt)?;
            }
            LaunchPath::Acp => {
                node.proc.send(&acp::initialize_request(1));
                node.await_response(1, BOOT)
                    .ok_or("no answer to ACP initialize")?;
                let session = launch
                    .session
                    .clone()
                    .unwrap_or_else(|| acp::session_new_request(2, &launch.inv.cwd, &[]));
                let id = session["id"].as_u64().unwrap_or(2);
                node.proc.send(&session);
                let answer = node
                    .await_response(id, BOOT)
                    .ok_or("no answer to session/new")?;
                if answer.get("error").is_some() {
                    return Err(format!("session refused: {}", answer["error"]));
                }
                node.session = match acp::session_id(&answer.to_string()) {
                    Ok(s) => Some(s),
                    // `session/load` answers without an id: the session is the one it named.
                    Err(_) => session["params"]["sessionId"].as_str().map(str::to_string),
                };
                node.prompt(prompt)?;
            }
        }
        Ok(node)
    }

    /// The JSON-RPC response to `id`, once it arrives.
    pub fn await_response(&self, id: u64, bound: Duration) -> Option<Value> {
        let mut found = None;
        self.proc.wait(bound, |io| {
            found = io
                .frames
                .iter()
                .find(|f| {
                    f.get("method").is_none() && f.get("id").and_then(Value::as_u64) == Some(id)
                })
                .cloned();
            found.is_some() || io.exit.is_some()
        });
        found
    }

    /// A new user message on the node's typed channel.
    pub fn prompt(&mut self, text: &str) -> Result<(), String> {
        match self.path {
            LaunchPath::Duplex => {
                self.proc.send_text(&duplex::user_message(text));
                Ok(())
            }
            LaunchPath::Acp => {
                let sid = self.session.clone().ok_or("no ACP session")?;
                self.next_id += 1;
                self.prompt_ids.push(self.next_id);
                self.proc
                    .send(&acp::prompt_request(self.next_id, &sid, text));
                Ok(())
            }
            LaunchPath::LaunchOnly | LaunchPath::Terminal => {
                Err("a launch-only surface takes no message after launch".into())
            }
        }
    }

    pub fn cancel(&self) {
        match self.path {
            LaunchPath::Acp => {
                if let Some(sid) = &self.session {
                    self.proc.send(&acp::cancel_notification(sid));
                }
            }
            LaunchPath::Duplex => {
                self.proc.send(&json!({"type": "control_request",
                "request_id": "marion-conformance-interrupt", "request": {"subtype": "interrupt"}}))
            }
            LaunchPath::LaunchOnly | LaunchPath::Terminal => self.proc.signal_group(SIGINT),
        }
    }

    /// Turns the node has finished: stream-json `result` frames, ACP prompt responses, or the
    /// process's exit for a launch-only node.
    pub fn ends(&self, io: &Io) -> usize {
        match self.path {
            LaunchPath::Duplex => io.frames.iter().filter(|f| f["type"] == "result").count(),
            LaunchPath::Acp => io
                .frames
                .iter()
                .filter(|f| {
                    f.get("method").is_none()
                        && f.get("id")
                            .and_then(Value::as_u64)
                            .is_some_and(|id| self.prompt_ids.contains(&id))
                })
                .count(),
            LaunchPath::LaunchOnly | LaunchPath::Terminal => usize::from(io.exit.is_some()),
        }
    }

    /// Wait until `n` turns have ended (or the process has exited).
    pub fn wait_ends(&self, n: usize, bound: Duration) -> bool {
        self.proc
            .wait(bound, |io| self.ends(io) >= n || io.exit.is_some())
            && self.proc.with(|io| self.ends(io) >= n)
    }

    /// The ACP prompt responses, in id order.
    pub fn prompt_answers(&self) -> Vec<(u64, Value)> {
        self.proc.with(|io| {
            self.prompt_ids
                .iter()
                .filter_map(|id| {
                    io.frames
                        .iter()
                        .find(|f| {
                            f.get("method").is_none()
                                && f.get("id").and_then(Value::as_u64) == Some(*id)
                        })
                        .map(|f| (*id, f.clone()))
                })
                .collect()
        })
    }
}
