//! **An id-correlated JSON-RPC peer over a child's stdio** — the one driver ACP and codex's
//! app-server share.
//!
//! Both protocols put a long-lived server on the child's stdin/stdout, one JSON frame per line:
//! marion's requests carry an id the answer echoes, the server's notifications carry none, and the
//! server sends requests of its own mid-turn that stall the turn until marion answers them. What the
//! two do *not* share is what the requests and notifications mean, so that is a [`Peer`] and
//! everything else is here: the spawn into its own process group, the reader, the id-correlated
//! wait, the answering, and the shutdown that kills the group.
//!
//! # Waits are events, not a tick
//!
//! The reader thread signals a condvar on every line and at EOF, and the node's inbox signals the
//! same condvar through [`Driver::wake_port`], so a wait wakes when there is something to read, a
//! message to deliver, or its deadline — plus one coarse [`LIVENESS_TICK`] to notice a server that
//! died while something it started still holds its stdout open.

use std::io::{BufRead, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

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

/// The reader's side: every line so far, whether stdout has ended, and whether the inbox woke the
/// driver since it last looked.
#[derive(Default)]
struct Feed {
    lines: Vec<String>,
    eof: bool,
    woken: bool,
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

/// The node's inbox's [`crate::inbox::DeliveryPort`] on a driver: a wake is one more event on the
/// condvar every wait blocks on.
struct Wake(Arc<Shared>);

impl crate::inbox::DeliveryPort for Wake {
    fn wake(&self) {
        self.0.lock().woken = true;
        self.0.cv.notify_all();
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
        // deadlock. `kill_process_tree` refuses marion's own pgid, so without a group of its own
        // the sweep would have nothing it may address.
        cmd.process_group(0);
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
            peer,
        })
    }

    /// The port a node's inbox wakes this driver through.
    pub fn wake_port(&self) -> Arc<dyn crate::inbox::DeliveryPort> {
        Arc::new(Wake(Arc::clone(&self.shared)))
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

    /// Block until there is an unread line, an inbox wake, EOF, or `until` — whichever is first —
    /// capped at [`LIVENESS_TICK`].
    pub fn wait_event(&self, until: Instant) {
        let until = until.min(Instant::now() + LIVENESS_TICK);
        let mut feed = self.shared.lock();
        while feed.lines.len() <= self.cursor && !feed.woken && !feed.eof {
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

    /// Classify, then ask `done`, on every wake until it answers `Some`, the deadline passes, or the
    /// server has been dead for [`DEATH_GRACE`] — telling "it crashed" from "it hung", which §8
    /// insists are two findings.
    pub fn settle_until<T>(
        &mut self,
        deadline: Instant,
        mut done: impl FnMut(&mut Self) -> Option<T>,
    ) -> Option<T> {
        let mut gone: Option<Instant> = None;
        loop {
            self.classify();
            if let Some(v) = done(self) {
                return Some(v);
            }
            if matches!(self.child.try_wait(), Ok(Some(_))) {
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
        self.stdin.take();
        if matches!(self.child.try_wait(), Ok(None)) {
            unsafe { kill(self.pid, SIGINT) };
        }
        let status = crate::wake::wait_bounded(&mut self.child, INTERRUPT_GRACE);
        let ended_on = if status.is_some() { SIGINT } else { SIGKILL };
        if status.is_none() {
            kill_process_tree(self.pid);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // Unconditional, and it is what makes the reader terminate: the server is reaped by here,
        // but what it started — marion's own MCP bridge, holding both pipes' write ends — is not.
        // The group survives its leader, so `kill(-pgid)` still addresses them.
        kill_process_tree(self.pid);

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
        if matches!(self.child.try_wait(), Ok(None)) {
            unsafe { kill(self.pid, SIGKILL) };
            let _ = self.child.wait();
        }
        // The group, for the reason `finish` sweeps it: the server's children hold its pipes.
        kill_process_tree(self.pid);
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
