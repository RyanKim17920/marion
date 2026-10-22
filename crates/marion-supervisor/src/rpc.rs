//! **An id-correlated JSON-RPC peer over a child's stdio** — the one driver ACP and codex's
//! app-server share.
//!
//! Both protocols put a long-lived server on the child's stdin/stdout, one JSON frame per line:
//! marion's requests carry an id the answer echoes, the server's notifications carry none, and the
//! server sends requests of its own mid-turn that stall the turn until marion answers them. What the
//! two do *not* share is what the requests and notifications mean, so that is a `Peer` and
//! everything else is here: the spawn into its own process group, the reader, the id-correlated
//! wait, the answering, and the shutdown that kills the group.
//!
//! # Waits are events, not a tick
//!
//! The reader thread signals a condvar on every line and at EOF, and the node's inbox signals the
//! same condvar through `Driver::wake_port`, so a wait wakes when there is something to read, a
//! message to deliver, or its deadline — plus one coarse `LIVENESS_TICK` to notice a server that
//! died while something it started still holds its stdout open.

use std::io::{BufRead, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use marion_harness::spec::AbortVerb;
use marion_harness::{ChildExit, Invocation};

use crate::kill::{DRAIN_GRACE, kill_process_tree};
use crate::run::Drain;

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

pub(crate) const SIGINT: i32 = 2;
pub(crate) const SIGKILL: i32 = 9;

/// How long the server is given to go away on SIGINT before its process group is killed. doctor's
/// number: §8 calls a binary that hangs on interrupt a distinct finding from one that is absent.
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);

/// The longest line a server may write that marion keeps as a frame. A line is buffered whole
/// before it is a frame at all, so without a bound one newline-free write grows marion's memory
/// until the wall clock ends the turn; a line past this is read to its end and dropped.
pub(crate) const MAX_LINE: usize = 32 * 1024 * 1024;

/// Every line of `r`, as `BufRead::lines` yields them (`\n` or `\r\n` stripped), handed to
/// `each`, except that a line longer than `max` bytes is discarded as it is read rather than
/// buffered. Stops at EOF, a read error, or a line that is not UTF-8, as `lines` does.
pub(crate) fn bounded_lines(mut r: impl BufRead, max: usize, mut each: impl FnMut(String)) {
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let (used, done) = match r.fill_buf() {
            // A last line with no newline is still a line, as `lines` has it.
            Ok([]) if line.is_empty() || oversized => return,
            Ok([]) => (0, true),
            Ok(buf) => match buf.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    if !oversized {
                        line.extend_from_slice(&buf[..i]);
                    }
                    (i + 1, true)
                }
                None => {
                    if !oversized {
                        line.extend_from_slice(buf);
                    }
                    (buf.len(), false)
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        r.consume(used);
        if line.len() > max {
            oversized = true;
            line = Vec::new();
        }
        if done && !std::mem::take(&mut oversized) {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            match String::from_utf8(std::mem::take(&mut line)) {
                Ok(text) => each(text),
                Err(_) => return,
            }
        }
    }
}

/// How long after the server's process is gone marion keeps reading its pipe before concluding an
/// answer is never coming. Not zero: the frames cross a pipe and a mutex after the exit status is
/// readable, so a server that wrote its last frame and exited in one breath is still being read.
const DEATH_GRACE: Duration = Duration::from_millis(250);

/// The longest a wait sleeps without a line, a wake or its deadline: the one check that is not an
/// event — whether the server died while a process it started still holds its stdout open. A dead
/// server almost always ends its stdout too (its children lose their stdin and exit), so this is a
/// safety bound, in seconds, and never the path an ordinary session takes.
const LIVENESS_TICK: Duration = Duration::from_secs(2);

/// What marion does with the frames a server sends that are not answers to marion's requests.
pub(crate) trait Peer {
    /// The reply to a request the server sent. Answered, never dropped: an unanswered request stops
    /// the turn dead until the wall clock kills it.
    fn answer(&mut self, request: &Value) -> Value;
    /// A notification — a frame with no id.
    fn notified(&mut self, _frame: &Value) {}
}

/// The reader's side: every line so far, whether stdout has ended, whether the inbox woke the
/// driver since it last looked, and whether the node's cancel reached it.
#[derive(Default)]
struct Feed {
    lines: Vec<String>,
    eof: bool,
    /// Raised by every wake until [`Driver::take_wake`] clears it: whether the inbox has something
    /// a caller may want to act on.
    woken: bool,
    /// Raised by every wake until a wait returns on it: a wake ends **one** wait, so a caller that
    /// leaves [`Self::woken`] up (a non-folding agent's turn ignores a mid-turn steer) still blocks
    /// on its next wait instead of returning at once until the turn settles.
    unseen: bool,
    /// Sticky: the driver notices it once ([`Driver::aborted`]), and a wait never wakes on it again.
    aborted: bool,
}

#[derive(Default)]
struct Shared {
    feed: Mutex<Feed>,
    cv: Condvar,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Feed> {
        self.feed.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The node's inbox's [`crate::inbox::DeliveryPort`] on a driver: a wake, and a cancel, are one
/// more event on the condvar every wait blocks on.
struct Wake {
    shared: Arc<Shared>,
    /// The row's headless verb is [`AbortVerb::Channel`], which this lane sends; without it a
    /// cancel is the canceller's kill, and the port says so.
    aborts: bool,
}

impl crate::inbox::DeliveryPort for Wake {
    fn wake(&self) {
        let mut feed = self.shared.lock();
        feed.woken = true;
        feed.unseen = true;
        drop(feed);
        self.shared.cv.notify_all();
    }

    fn abort(&self) -> bool {
        if !self.aborts {
            return false;
        }
        self.shared.lock().aborted = true;
        self.shared.cv.notify_all();
        true
    }
}

/// How long a turn ended early is given to close, where the row's headless verb is the typed
/// channel's own interrupt — the lane's to send — and `None` for a row that has none.
pub(crate) fn channel_grace(verb: AbortVerb) -> Option<Duration> {
    match verb {
        AbortVerb::Channel { grace_ms, .. } => Some(Duration::from_millis(u64::from(grace_ms))),
        AbortVerb::Keys { .. } | AbortVerb::None { .. } => None,
    }
}

/// What a driven session leaves behind, in the three fields every marion child path returns.
#[derive(Debug)]
pub struct RpcRun {
    /// Every frame the server wrote, one per line, verbatim and in arrival order — including lines
    /// that are not JSON, because the row's reader is given what the server wrote and decides for
    /// itself. This is the string the adapter's reader consumes.
    pub stdout: String,
    pub stderr: String,
    /// **Deliberately not the turn's verdict.** A stdio server is shut down by marion once the
    /// session settles, so this describes marion's shutdown; what describes the turn is the frames
    /// ([`turn_exit`] is the node's exit).
    pub exit: ChildExit,
    /// A pipe was still open when the drain bound expired, so the capture is a prefix (§6.7:
    /// recorded, never silent).
    pub capture_truncated: bool,
    /// marion ended the session on a frame the row reads as final — a refused credential no retry
    /// heals — for this reason. `None` on every session that ended any other way.
    pub stopped: Option<String>,
    /// Where in [`Self::stdout`] the last prompt marion sent begins to be answered — 0 until a
    /// second one is sent. What the server wrote from here on is what it said in that turn.
    pub last_turn_at: usize,
}

/// A spawned server with its stdout read line by line and its stderr drained, so a server request
/// can be answered while frames are still arriving.
///
/// **Every exit from this struct kills the server, and kills its group.** A stdio server never
/// closes stdout on its own, and its own children (marion's MCP bridge among them) inherit its
/// pipes. [`Drop`] is what makes that true on the paths that return an error; [`Driver::finish`] is
/// the one that also reports what the kill took.
pub(crate) struct Driver<'a, P> {
    pub child: Child,
    pub pid: i32,
    stdin: Option<ChildStdin>,
    shared: Arc<Shared>,
    stderr: Option<Drain>,
    /// How far into the lines [`Self::classify`] has read. Requests are answered once and responses
    /// indexed once, however many times a wait wakes.
    cursor: usize,
    /// How many lines preceded the last prompt marion sent after the first: the server's answer to
    /// its latest turn is every line from here on.
    pub last_turn_line: usize,
    /// `(id, frame)` for every response the server has sent. Kept because notifications arrive
    /// interleaved with, and before, the response they belong to (S21), so an answer that lands
    /// while marion waits on an earlier id must still be there when marion asks for it.
    pub responses: Vec<(u64, String)>,
    /// Called with every line, verbatim, in arrival order, exactly once.
    on_line: Option<&'a dyn Fn(&str)>,
    /// The node's cancel reached this driver ([`Self::aborted`]).
    aborted: bool,
    pub peer: P,
}

impl<'a, P: Peer> Driver<'a, P> {
    /// Spawn `inv` in **its own process group** — what makes the kill able to reach the server's
    /// own children, which inherit its stdout write end.
    pub fn spawn(
        inv: &Invocation,
        tmpdir: &std::path::Path,
        on_line: Option<&'a dyn Fn(&str)>,
        peer: P,
    ) -> std::io::Result<Self> {
        let mut cmd = inv.command(tmpdir);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // A read on a pipe returns EOF only when every write end is closed, so killing the server
        // alone would leave the reader wedged on its children's copies — `duplex` measured that
        // deadlock. `kill_process_tree` refuses marion's own pgid, so without a group (and a session)
        // of its own the sweep would have nothing it may address.
        crate::kill::lead_own_session(&mut cmd);
        crate::node_limits::apply(&mut cmd);
        let mut child = crate::spawn_receive_gate::SPAWN_RECEIVE_GATE.spawn(&mut cmd)?;
        let pid = child.id() as i32;
        let stdin = child.stdin.take();
        let shared = Arc::new(Shared::default());
        let out = child.stdout.take().expect("stdout was piped");
        let sink = Arc::clone(&shared);
        // Detached, and it has to be: the join is what would deadlock if anything the server
        // started outlives it, so the group kill is this thread's exit condition. It blocks in
        // `read`, so it costs nothing while the server is quiet.
        std::thread::spawn(move || {
            bounded_lines(std::io::BufReader::new(out), MAX_LINE, |line| {
                sink.lock().lines.push(line);
                sink.cv.notify_all();
            });
            sink.lock().eof = true;
            sink.cv.notify_all();
        });
        let stderr = Drain::start(child.stderr.take().expect("stderr was piped"));
        Ok(Self {
            child,
            pid,
            stdin,
            shared,
            stderr: Some(stderr),
            cursor: 0,
            last_turn_line: 0,
            responses: Vec::new(),
            on_line,
            aborted: false,
            peer,
        })
    }

    /// The port a node's inbox wakes this driver through. `abort` is the row's headless verb: the
    /// port takes a cancel only where the lane can send it ([`channel_grace`]).
    pub fn wake_port(&self, abort: AbortVerb) -> Arc<dyn crate::inbox::DeliveryPort> {
        Arc::new(Wake {
            shared: Arc::clone(&self.shared),
            aborts: channel_grace(abort).is_some(),
        })
    }

    /// **Whether the node is being cancelled**: its inbox's [`crate::inbox::DeliveryPort::abort`]
    /// reached this driver. Every [`Self::settle_until`] stops on it — answering `None`, as at a
    /// deadline — once; a caller tells the two apart here and ends the turn by the row's verb.
    pub fn aborted(&self) -> bool {
        self.aborted
    }

    /// `true` the first time the cancel is seen; from then on [`Self::aborted`] says so. For a wait
    /// that is not a [`Self::settle_until`].
    pub fn take_abort(&mut self) -> bool {
        if self.aborted || !self.shared.lock().aborted {
            return false;
        }
        self.aborted = true;
        true
    }

    /// Whether the inbox woke the driver since the last take, clearing it.
    pub fn take_wake(&self) -> bool {
        std::mem::take(&mut self.shared.lock().woken)
    }

    /// One frame, newline-terminated. `serde_json`'s compact form never contains a newline.
    pub fn write(&mut self, frame: &Value) -> std::io::Result<()> {
        let Some(w) = self.stdin.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "marion has already closed the server's stdin",
            ));
        };
        writeln!(w, "{frame}")?;
        w.flush()
    }

    /// Block until there is an unread line, an inbox wake no wait has returned on, EOF, or `until`
    /// — whichever is first — capped at [`LIVENESS_TICK`]. Once stdout has ended nothing more can
    /// be read, so the wait is on the server's exit instead ([`crate::wake::wait_exit`]), never a
    /// return at once that would make its caller's loop spin.
    pub fn wait_event(&mut self, until: Instant) {
        let until = until.min(Instant::now() + LIVENESS_TICK);
        if self.stdout_ended() {
            // Unreaped: `finish` sweeps the session while the pid still pins its id.
            crate::wake::wait_exit(&self.child, until.saturating_duration_since(Instant::now()));
            return;
        }
        let seen = self.aborted;
        let mut feed = self.shared.lock();
        while feed.lines.len() <= self.cursor
            && !feed.unseen
            && !feed.eof
            && (seen || !feed.aborted)
        {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            feed = self
                .shared
                .cv
                .wait_timeout(feed, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        feed.unseen = false;
    }

    /// Wait for the response carrying `id`, **answering every server request that arrives in the
    /// meantime**. `None` is "not by the deadline", or the server died without answering.
    pub fn settle(&mut self, id: u64, deadline: Instant) -> Option<String> {
        self.settle_while(id, deadline, |_| {})
    }

    /// [`Self::settle`], calling `between` on every wake — where a driver sends what it may send
    /// while the server is still working (a folded message).
    pub fn settle_while(
        &mut self,
        id: u64,
        deadline: Instant,
        mut between: impl FnMut(&mut Self),
    ) -> Option<String> {
        self.settle_until(deadline, |d| {
            between(d);
            d.responses
                .iter()
                .find(|(k, _)| *k == id)
                .map(|(_, f)| f.clone())
        })
    }

    /// Classify, then ask `done`, on every wake until it answers `Some`, the deadline passes, the
    /// node's cancel arrives ([`Self::aborted`]), or the server has been dead for [`DEATH_GRACE`] —
    /// telling "it crashed" from "it hung", which §8 insists are two findings.
    pub fn settle_until<T>(
        &mut self,
        deadline: Instant,
        mut done: impl FnMut(&mut Self) -> Option<T>,
    ) -> Option<T> {
        let mut gone: Option<Instant> = None;
        loop {
            // Read before the classify: every line precedes EOF, so this pass sees all there is.
            let ended = self.stdout_ended();
            self.classify();
            if let Some(v) = done(self) {
                return Some(v);
            }
            if self.take_abort() {
                return None;
            }
            if ended {
                // No answer can arrive. Wait for the process itself, so a caller that reads its
                // exit next sees it — a server closes stdout a moment before it goes.
                crate::wake::wait_exit(
                    &self.child,
                    deadline.saturating_duration_since(Instant::now()),
                );
                return None;
            }
            if self.exited() {
                match gone {
                    Some(t) if t.elapsed() >= DEATH_GRACE => return None,
                    Some(_) => {}
                    None => gone = Some(Instant::now()),
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            let until = match gone {
                Some(t) => deadline.min(t + DEATH_GRACE),
                None => deadline,
            };
            self.wait_event(until);
        }
    }

    /// Read every frame the server has written since the last call: index the responses, answer the
    /// requests, hand the notifications to the peer.
    pub fn classify(&mut self) {
        let new: Vec<String> = {
            let feed = self.shared.lock();
            if feed.lines.len() <= self.cursor {
                return;
            }
            let from = self.cursor;
            self.cursor = feed.lines.len();
            feed.lines[from..].to_vec()
        };
        for line in new {
            // Before the parse: a watcher sees what the server wrote, banners included.
            if let Some(sink) = self.on_line {
                sink(&line);
            }
            // A line that is not JSON stays in the transcript verbatim and is classified as nothing;
            // inventing a reply to a banner would be worse than ignoring it.
            let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let has_id = frame.get("id").is_some_and(|v| !v.is_null());
            if has_id && frame.get("method").is_some() {
                let reply = self.peer.answer(&frame);
                // Best-effort: a server whose stdin has closed is being shut down anyway.
                let _ = self.write(&reply);
            } else if has_id && (frame.get("result").is_some() || frame.get("error").is_some()) {
                if let Some(k) = frame.get("id").and_then(Value::as_u64) {
                    self.responses.push((k, line));
                }
            } else {
                self.peer.notified(&frame);
            }
        }
    }

    /// Shut the server down and collect everything: EOF on stdin, SIGINT, the group killed.
    ///
    /// `marion_cut_it_short` is the caller's own statement that the session did not finish — what
    /// §6.7 calls an attributed kill, not something read off a signal number.
    pub fn finish(&mut self, marion_cut_it_short: bool) -> (RpcRun, usize) {
        // **Enumerated while the server still lives.** A server that ends on stdin EOF leaves the
        // commands it started — codex's `exec_command` runs each in a session of its own — to pid 1,
        // in groups no walk from a reaped server reaches.
        let below = crate::kill::groups_below(self.pid);
        self.stdin.take();
        if matches!(crate::wake::has_exited(&self.child), Ok(false)) {
            unsafe { kill(self.pid, SIGINT) };
        }
        // Waited on unreaped: the server's pid, and so its session id, stays its own until the
        // session is swept below.
        let exited = crate::wake::wait_exit(&self.child, INTERRUPT_GRACE);
        let ended_on = if exited { SIGINT } else { SIGKILL };
        if !exited {
            kill_process_tree(self.pid);
        }
        // Unconditional, and it is what makes the reader terminate: what the server started —
        // marion's own MCP bridge, holding both pipes' write ends — outlives it in its session.
        // Swept, then the server reaped (`end_node`).
        let reaped = crate::kill::end_node(&mut self.child).ok();
        let status = reaped.filter(|_| exited);
        crate::kill::kill_groups(&below);

        let drain_deadline = Instant::now() + DRAIN_GRACE;
        let (collected, stdout_complete) = {
            let mut feed = self.shared.lock();
            while !feed.eof {
                let left = drain_deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                feed = self
                    .shared
                    .cv
                    .wait_timeout(feed, left)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            (feed.lines.clone(), feed.eof)
        };
        // The lines no wait classified, so a watcher's view ends where the transcript does.
        if let Some(sink) = self.on_line {
            collected.iter().skip(self.cursor).for_each(|l| sink(l));
        }
        self.cursor = collected.len();
        let (stderr, stderr_complete) = match self.stderr.take() {
            Some(d) => d.finish(drain_deadline),
            None => (Vec::new(), true),
        };

        // **Marion's kill, reported as marion's kill.** A server that raced the shutdown and exited
        // 0 would otherwise hand a reader an exit 0 for a session marion cut off.
        let exit = if marion_cut_it_short {
            ChildExit {
                code: None,
                signal: Some(ended_on),
                timed_out: true,
            }
        } else {
            ChildExit {
                code: status.as_ref().and_then(std::process::ExitStatus::code),
                signal: status
                    .as_ref()
                    .and_then(ExitStatusExt::signal)
                    .or((status.is_none()).then_some(SIGKILL)),
                timed_out: false,
            }
        };
        let stdout = collected.join("\n");
        (
            RpcRun {
                last_turn_at: line_offset(&stdout, self.last_turn_line),
                stdout,
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
                exit,
                capture_truncated: !(stdout_complete && stderr_complete),
                stopped: None,
            },
            collected.len(),
        )
    }

    /// Whether the server's process has exited, read without reaping it: [`Self::finish`] sweeps its
    /// session while the unreaped pid still pins the session's id.
    pub fn exited(&self) -> bool {
        matches!(crate::wake::has_exited(&self.child), Ok(true))
    }

    /// Whether the server's stdout has ended: every line it will ever write has been read.
    pub fn stdout_ended(&self) -> bool {
        self.shared.lock().eof
    }

    /// How many lines the server has written so far.
    pub fn frame_count(&self) -> usize {
        self.shared.lock().lines.len()
    }

    /// Every line the server has written from line `from` on, as one stretch of its stream.
    pub fn frames_since(&self, from: usize) -> String {
        let feed = self.shared.lock();
        feed.lines.get(from..).unwrap_or_default().join("\n")
    }

    /// Shut down and build a refusal out of what the server left behind. Its stderr is the only
    /// thing an operator can act on when the frames say nothing, so no error path may skip it.
    pub fn refuse<E>(&mut self, make: impl FnOnce(RpcRun, usize) -> E) -> E {
        let (end, frames) = self.finish(true);
        make(end, frames)
    }
}

impl<P> Drop for Driver<'_, P> {
    fn drop(&mut self) {
        // Before the server is killed, for `finish`'s reason — and only while it runs: a reaped
        // server's pid may already name someone else's process.
        let running = matches!(crate::wake::has_exited(&self.child), Ok(false));
        let below = if running {
            crate::kill::groups_below(self.pid)
        } else {
            Vec::new()
        };
        if running {
            kill_process_tree(self.pid);
        }
        // The session, for the reason `finish` sweeps it: the server's children hold its pipes.
        // Nothing is signalled once `finish` has reaped the server.
        let _ = crate::kill::end_node(&mut self.child);
        crate::kill::kill_groups(&below);
    }
}

/// A step's deadline: its own budget, clipped to what is left of the caller's wall clock — never
/// the later of the two, which would make the wall clock advisory.
pub(crate) fn clip(overall: Instant, budget: Duration) -> Instant {
    let step = Instant::now()
        .checked_add(budget)
        .unwrap_or_else(|| Instant::now() + budget / 2);
    step.min(overall)
}

/// The last `max` bytes of `s`, trimmed: a process that failed at startup says why in its final
/// lines and buries them under whatever it logged on the way there.
pub(crate) fn excerpt(s: &str, max: usize) -> String {
    let s = s.trim_end();
    if s.len() <= max {
        return if s.is_empty() {
            "<empty>".into()
        } else {
            s.into()
        };
    }
    let cut = s.len() - max;
    let cut = (cut..s.len())
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(s.len());
    format!("…{}", &s[cut..])
}

/// **The session's end, not the process's**, as a node's recorded exit: code 0 for a session the
/// server answered, and no code at all for one marion cut short on its bound. A stdio server is
/// shut down by marion once the session settles, so its process status describes marion's shutdown,
/// and recorded as the node's exit it would read as a node killed by a signal on every ordinary run.
pub fn turn_exit(exit: ChildExit) -> ChildExit {
    ChildExit {
        code: (!exit.timed_out).then_some(0),
        signal: None,
        timed_out: exit.timed_out,
    }
}

/// The byte offset in `text` at which its line `n` begins (0-based), or its length when it has
/// fewer lines.
fn line_offset(text: &str, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    text.match_indices('\n')
        .nth(n - 1)
        .map_or(text.len(), |(i, _)| i + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &[u8], max: usize, cap: usize) -> Vec<String> {
        let mut out = Vec::new();
        bounded_lines(std::io::BufReader::with_capacity(cap, input), max, |l| {
            out.push(l)
        });
        out
    }

    struct Mute;
    impl Peer for Mute {
        fn answer(&mut self, _request: &Value) -> Value {
            Value::Null
        }
    }

    fn sh(script: &str, cwd: &std::path::Path) -> Invocation {
        Invocation {
            inherit: None,
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![],
            env_remove: vec![],
            cwd: cwd.to_path_buf(),
            model: None,
            session_mode: None,
            sandbox: None,
        }
    }

    /// CPU time this thread has used: a wait that spins shows up here, one that blocks does not.
    fn thread_cpu() -> Duration {
        #[repr(C)]
        struct Timespec {
            sec: i64,
            nsec: i64,
        }
        unsafe extern "C" {
            fn clock_gettime(clock: i32, tp: *mut Timespec) -> i32;
        }
        #[cfg(target_os = "macos")]
        const CLOCK_THREAD_CPUTIME_ID: i32 = 16;
        #[cfg(not(target_os = "macos"))]
        const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
        let mut t = Timespec { sec: 0, nsec: 0 };
        // SAFETY: `t` is a valid timespec for the call to fill.
        assert_eq!(unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut t) }, 0);
        Duration::new(t.sec as u64, t.nsec as u32)
    }

    /// **What a server left in its session ends with the node**, as on a launch-only or duplex node:
    /// the server forks a child into a group of its own that holds no pipe and exits by itself,
    /// and is seen to exit (unreaped) before [`Driver::finish`] sweeps the session and reaps it.
    #[test]
    fn a_child_a_server_left_in_its_session_ends_with_the_node() {
        let dir = marion_testsupport::scratch("rpc-session-orphan");
        let pids = dir.join("pids");
        let fork = dir.join("fork.pl");
        std::fs::write(
            &fork,
            format!(
                r#"if (fork() == 0) {{
                       setpgrp(0, 0); close(STDIN); close(STDOUT); close(STDERR);
                       open(my $f, ">>", "{p}"); print $f "$$\n"; close $f;
                       sleep 900; exit 0;
                   }}
                   for (1..200) {{ last if -s "{p}"; select(undef, undef, undef, 0.01); }}"#,
                p = pids.display()
            ),
        )
        .unwrap();
        let script = format!("perl {}", fork.display());
        let mut d = Driver::spawn(&sh(&script, &dir), &dir, None, Mute).expect("sh");
        assert!(marion_testsupport::until(|| d.exited()), "the server exits");
        let _ = d.finish(false);
        let left: Vec<i32> = std::fs::read_to_string(&pids)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect();
        let alive: Vec<i32> = left
            .iter()
            .copied()
            .filter(|p| {
                !matches!(
                    crate::procid::run_state(*p),
                    crate::procid::RunState::Gone | crate::procid::RunState::Zombie
                )
            })
            .collect();
        for p in &left {
            marion_testsupport::kill_hard(*p);
        }
        assert_eq!(left.len(), 1, "the child recorded itself");
        assert!(
            alive.is_empty(),
            "left running after the node ended: {alive:?}"
        );
    }

    /// **A server whose stdout has ended is waited on by its exit, not re-asked in a loop.** EOF
    /// means no answer can arrive, so the wait blocks on the process until it goes or the deadline
    /// passes, and the caller then sees its exit.
    #[test]
    fn a_server_that_closed_its_stdout_is_waited_on_without_spinning() {
        let dir = marion_testsupport::scratch("rpc-eof");
        // Closes stdout and lives on past the deadline; then one that exits a moment after.
        for (script, exits) in [
            ("exec 1>&-; sleep 3", false),
            ("exec 1>&-; sleep 0.3", true),
        ] {
            let mut d = Driver::spawn(&sh(script, &dir), &dir, None, Mute).expect("sh");
            let cpu = thread_cpu();
            let started = Instant::now();
            assert_eq!(
                d.settle(1, Instant::now() + Duration::from_millis(1200)),
                None
            );
            let (spent, took) = (thread_cpu() - cpu, started.elapsed());
            assert!(
                spent < Duration::from_millis(150),
                "{script}: spun for {spent:?} of CPU"
            );
            assert_eq!(d.exited(), exits, "{script}");
            if exits {
                assert!(
                    took < Duration::from_millis(1000),
                    "{script}: waited {took:?}"
                );
            }
        }
    }

    /// **A wake no caller takes ends one wait, not every one after it.** A non-folding agent's turn
    /// leaves a mid-turn steer in the inbox for the next turn, and so never takes its wake; the
    /// turn's wait must still block until the server answers, rather than spin until it does.
    #[test]
    fn a_wake_left_untaken_ends_one_wait_and_the_next_blocks() {
        let dir = marion_testsupport::scratch("rpc-untaken-wake");
        let mut d = Driver::spawn(&sh("sleep 3", &dir), &dir, None, Mute).expect("sh");
        d.wake_port(AbortVerb::None { note: "test" }).wake();
        let (cpu, mut passes) = (thread_cpu(), 0);
        let settled = d.settle_until(Instant::now() + Duration::from_millis(1200), |_| {
            passes += 1;
            None::<()>
        });
        let spent = thread_cpu() - cpu;
        assert_eq!(settled, None);
        assert_eq!(
            passes, 3,
            "the first pass, the one the wake ended, the one at the deadline"
        );
        assert!(
            spent < Duration::from_millis(150),
            "spun for {spent:?} of CPU"
        );
        assert!(
            d.take_wake(),
            "the wake is still there for a caller to take"
        );
        let _ = d.finish(false);
    }

    /// **A wake still ends a wait that is blocked**, so the one-wait rule never costs a caller the
    /// wake it is waiting for.
    #[test]
    fn a_wake_ends_a_blocked_wait() {
        let dir = marion_testsupport::scratch("rpc-wake-blocked");
        let mut d = Driver::spawn(&sh("sleep 3", &dir), &dir, None, Mute).expect("sh");
        let port = d.wake_port(AbortVerb::None { note: "test" });
        let raise = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            port.wake();
        });
        let started = Instant::now();
        let settled = d.settle_until(Instant::now() + Duration::from_millis(1800), |d| {
            d.take_wake().then_some(())
        });
        let took = started.elapsed();
        raise.join().unwrap();
        assert_eq!(settled, Some(()));
        assert!(took < Duration::from_millis(1000), "woken after {took:?}");
        let _ = d.finish(false);
    }

    /// **A server's line is bounded**: one past `max` is dropped whole, however it arrives in
    /// reads, and the lines around it survive exactly as `BufRead::lines` would give them.
    #[test]
    fn a_line_past_the_bound_is_dropped_and_its_neighbours_kept() {
        let long = "x".repeat(100);
        let input = format!("{{\"a\":1}}\r\n{long}\n{{\"b\":2}}\nlast");
        for cap in [1, 7, 64, 4096] {
            assert_eq!(
                lines(input.as_bytes(), 50, cap),
                ["{\"a\":1}", "{\"b\":2}", "last"],
                "buffer {cap}"
            );
            assert_eq!(
                lines(input.as_bytes(), 100, cap),
                ["{\"a\":1}", long.as_str(), "{\"b\":2}", "last"],
                "a line exactly at the bound is kept (buffer {cap})"
            );
        }
        assert_eq!(
            lines(b"ok\n\xff\nnever\n", 50, 8),
            ["ok"],
            "not UTF-8 ends it"
        );
    }
}
