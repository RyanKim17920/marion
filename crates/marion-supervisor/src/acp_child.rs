//! §6.1 step 8 for the fifth harness: driving a spawned ACP agent through one real turn.
//!
//! marion can put a node on four harnesses. The fifth — §5.2's `acp` row — is compiled all the way
//! to an [`Invocation`] and an MCP declaration by `AcpAdapter`, and then has nowhere to go:
//! `duplex::launch_path` maps every `ControlTransport::Typed(_)` to `LaunchPath::Duplex`, and
//! `duplex::run_duplex` speaks claude-code's `stream-json`. An ACP agent handed a `stream-json`
//! frame answers nothing, so the declaration that S21 proved carries marion's bridge has never
//! reached a node. This module is the missing driver.
//!
//! # What it is, and what it is a second copy of
//!
//! The mechanism is `doctor`'s: `AcpChild`/`acp_live_turn` already take a real `opencode acp` from
//! spawn to `end_turn` from the shipped `marion-supervisor doctor` binary, and nothing here is a
//! new idea about ACP. What differs is what the two are *for*, and the differences are not
//! cosmetic:
//!
//! * **doctor probes; this runs a node.** doctor's `session/new` deliberately carries
//!   [`McpDeclaration::None`] so marion is not on both ends of its own assertion (§8). A node run
//!   carries the adapter's real declaration — the whole point of the fifth `McpRoute` — so the id
//!   this driver correlates its answer against is read *out of the declaration the adapter built*
//!   rather than assumed, and a declaration that is not a `session/new` request is refused before
//!   anything is spawned.
//! * **doctor answers nothing the agent asks; a node run must.** doctor's micro-prompt is chosen so
//!   the turn never needs a permission or a file, and `AcpChild` consequently has no inbound
//!   request path at all. A real prompt does need them — S21's own probe grew
//!   `answer_agent_requests` for exactly this — and an unanswered client-bound request hangs the
//!   turn until the wall clock kills it. Worse, the shipped [`acp::initialize_request`] advertises
//!   `fs.readTextFile`, `fs.writeTextFile` and `terminal`, so marion has already told the agent it
//!   will answer three kinds of request before this driver sees the first one.
//! * **doctor reports prose; this returns a transcript.** The three fields every other child path
//!   returns, with `stdout` being the agent's own frames verbatim so `acp::parse_stream` and
//!   `acp::marion_calls` read what the agent wrote and not a re-serialisation of it.
//!
//! # The transcript is the agent's stdout and nothing else
//!
//! `tests/fixtures/s21/opencode-acp-session.jsonl` is one direction only — every line in it is a
//! frame `opencode acp` wrote — and `acp::json_frames` is a line splitter with no notion of
//! direction. Interleaving marion's own requests into `stdout` here would put frames carrying
//! `"method":"session/prompt"` in front of a reader that scans every frame for `result.stopReason`
//! and for `session/update` tool calls. It would not currently misread one, which is precisely why
//! it must not be done on the strength of that: the reader is written against a capture of one
//! direction, so this produces one direction.
//!
//! # Threads, not a runtime
//!
//! The workspace has no async runtime and `agent-client-protocol` 2.0.0 is not a dependency. One
//! detached reader thread per pipe and a poll loop, exactly as `doctor` and `run_bounded` do it.

use std::io::{BufRead, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use marion_harness::{AgentHandshake, ChildExit, Invocation, acp};

use crate::run::{DRAIN_GRACE, Drain, kill_process_tree};

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

const SIGINT: i32 = 2;
const SIGKILL: i32 = 9;

/// The JSON-RPC id marion stamps on `initialize`. doctor's number, kept, so a capture taken with
/// one and read against the other still correlates.
const INITIALIZE_ID: u64 = 0;
/// The id on `session/prompt`. `session/new`'s is **not** a constant here — it is read out of the
/// declaration the adapter compiled (`marion_harness::adapter::SESSION_NEW_ID` is where it is
/// stamped), because a driver that assumed the value and an adapter that changed it would not fail
/// loudly; they would wait out the whole session budget for an answer that had already arrived
/// under a different id.
const PROMPT_ID: u64 = 2;

/// How long the agent is given to answer `initialize`. A process start and one frame.
///
/// S22 measured what makes this a budget and not a formality: `npx -y @agentclientprotocol/codex-acp`
/// was still *downloading* when 30 s expired, returned zero frames, and looked exactly like a dead
/// agent. The same version run from `node_modules/.bin` handshakes in under a second. So an expiry
/// here is reported as an expiry with the agent's stderr attached, never as "the agent is broken".
const HANDSHAKE_BUDGET: Duration = Duration::from_secs(30);

/// How long `session/new` is given. Longer than the handshake because S20 measured it reaching a
/// vendor over the network before answering — including to say no (`gemini --acp`'s `-32000`).
const SESSION_BUDGET: Duration = Duration::from_secs(60);

/// How long the cancelled turn is given to answer after `session/cancel`.
///
/// `session/cancel` is a notification: the pending `session/prompt` answers it with
/// `stopReason: "cancelled"` (§5.2). That answer is worth waiting a few seconds for — it is the
/// agent's own statement that it stopped, and it lands in the transcript where a reader can see it
/// — but it is not worth waiting for indefinitely, because an agent that ignores the cancel is
/// exactly the agent the kill below exists for.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// How long the agent is given to go away on SIGINT before the process group is killed. doctor's
/// number, for the same reason: §8 calls a binary that hangs on interrupt a distinct finding from a
/// binary that is absent.
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);

/// How often a wait wakes to re-read the frame sink.
const POLL: Duration = Duration::from_millis(20);

/// How long after the agent's process is gone marion keeps reading its pipe before concluding the
/// answer is never coming. Not zero: the frames are drained by another thread, so an agent that
/// wrote its last frame and exited in the same breath has a window in which the process is dead and
/// the answer is still in flight through a pipe and a mutex.
const DEATH_GRACE: Duration = Duration::from_millis(250);

/// How much of the agent's stderr a refusal carries. Enough to hold a stack trace's last words;
/// bounded because an error message is read by a person and an agent that fails at startup can
/// write megabytes.
const STDERR_EXCERPT: usize = 2000;

/// What a driven ACP turn leaves behind, in the three fields every marion child path returns.
#[derive(Debug)]
pub struct AcpRun {
    /// Every frame the agent wrote, one per line, verbatim and in arrival order — including lines
    /// that are not JSON, because `acp::parse_stream` is given what the agent wrote and decides for
    /// itself. This is the string the adapter's reader consumes.
    pub stdout: String,
    pub stderr: String,
    /// **Deliberately not the turn's verdict.** `acp::parse_stream` ignores it, and this driver is
    /// the reason it can: an ACP agent is a long-lived stdio server that marion kills, so this
    /// describes marion's shutdown. What describes the turn is `stopReason` and the frames.
    pub exit: ChildExit,
    /// A pipe was still open when the drain bound expired, so the capture is a prefix (§6.7:
    /// recorded, never silent).
    pub capture_truncated: bool,
}

/// One ACP turn, as plain data.
pub struct AcpChildSpec<'a> {
    /// Program, args, env and cwd, already compiled by `AcpAdapter::compile`.
    pub inv: &'a Invocation,
    /// The `session/new` **request** the adapter built (`HarnessAdapter::session_declaration`),
    /// whole — envelope, id and all — or `None` for a session with no MCP servers declared.
    ///
    /// The whole request rather than its params, because the id is half of what a driver needs and
    /// splitting the frame here would mean re-assembling it against a second idea of what the
    /// adapter stamped. `None` compiles `acp::session_new_request(_, cwd, &[])`: an **empty**
    /// `mcpServers`, which is the shape S21 sent on every probe, and never an absent key.
    pub session_declaration: Option<Value>,
    pub prompt: &'a str,
    /// The wall clock for the turn.
    ///
    /// It bounds the whole conversation, not each step: `initialize` and `session/new` take their
    /// own measured budgets clipped to whatever is left of this, and the prompt gets the remainder.
    /// The function returns within `bound` plus the shutdown grace (a cancel, an interrupt and a
    /// drain), which is bounded and small — never within `bound` exactly, because a kill that is
    /// not waited on is a leak.
    pub bound: Duration,
    /// Called with the pid the instant it is known, **before one byte is written to the node**.
    ///
    /// §6.1 step 7's confirmation is the caller's to write and this is the first instant it can be
    /// written truthfully. See the same call in `duplex::run_duplex`: putting it here rather than
    /// after the handshake means the window in which a process exists and no durable record names
    /// it is one append and one fsync wide, instead of the node's whole first turn.
    pub on_started: &'a dyn Fn(i32),
}

impl std::fmt::Debug for AcpChildSpec<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpChildSpec")
            .field("inv", &self.inv)
            .field("session_declaration", &self.session_declaration)
            .field("prompt", &self.prompt)
            .field("bound", &self.bound)
            .field("on_started", &"<on_started>")
            .finish()
    }
}

/// Why marion has no transcript.
///
/// Every variant names what marion asked for and did not get, and carries the agent's own words
/// where it has any — its stderr, or the JSON-RPC error it answered with. A turn that *ran* and
/// went badly is not in here: a refused `session/prompt`, a cancelled turn and a tool call the
/// agent marked failed are all facts in the transcript, and `acp::parse_stream` is what reads them.
/// This enum is only for the cases where there is nothing to read.
#[derive(Debug, thiserror::Error)]
pub enum AcpChildError {
    #[error("could not start the ACP agent `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    /// The pipe to the agent closed under marion's own write. Distinct from a silent agent: this
    /// one says the request never went out, so no budget below it was ever really spent.
    #[error("the agent's stdin closed while marion was writing `{step}`: {source}")]
    Unwritable {
        step: &'static str,
        #[source]
        source: std::io::Error,
    },
    /// The adapter handed over something that is not a `session/new` request. Checked before the
    /// agent is spawned, because the alternative is a live agent waiting on a frame it will never
    /// understand and a driver waiting on an id nobody stamped.
    #[error(
        "the session declaration marion compiled is not a `session/new` request: {what}. \
         `AcpAdapter::session_declaration` builds it with `acp::session_new_request`, and this \
         driver correlates the agent's answer against the id that request carries"
    )]
    Declaration { what: String },
    #[error(
        "the agent never answered `initialize` within {waited:?}, so marion never learned what it \
         is or what it can do and no session was opened. It wrote {frames} frames. Its stderr: \
         {stderr}"
    )]
    NoHandshake {
        waited: Duration,
        frames: usize,
        stderr: String,
    },
    /// It answered, and the answer is not one marion can key on — a wire version it does not speak,
    /// an anonymous agent, or a JSON-RPC error. `AcpError` already carries the agent's own words.
    #[error("the agent answered `initialize` with something marion cannot use: {0}")]
    Handshake(#[source] acp::AcpError),
    #[error(
        "the agent completed `initialize` and then never answered `session/new` within {waited:?}, \
         so there is no session to prompt. It wrote {frames} frames. Its stderr: {stderr}"
    )]
    NoSession {
        waited: Duration,
        frames: usize,
        stderr: String,
    },
    /// **The S20 blocker's shape**, and it is the agent's refusal rather than marion's failure to
    /// understand: `gemini --acp` answers `-32000`, *"This client is no longer supported for Gemini
    /// Code Assist for individuals"*. No adapter can route around it, so it is reported in the
    /// vendor's own sentence.
    #[error("the agent refused to open a session: {0}")]
    SessionRefused(#[source] acp::AcpError),
}

/// Drive one ACP turn and hand back the transcript.
///
/// The order is ACP's, and each step is bounded and refuses by name rather than falling through to
/// the next:
///
/// 1. spawn, in **its own process group** — the group is what makes the kill able to reach marion's
///    own MCP bridge, which the *agent* starts from the `session/new` declaration and which
///    inherits the agent's stdout write end;
/// 2. [`AcpChildSpec::on_started`], before one byte goes out;
/// 3. `initialize`, and a handshake marion can key on (§3.3's stage two);
/// 4. `session/new`, carrying the adapter's declaration, correlated on that request's own id;
/// 5. `session/prompt`, answering the agent's client-bound requests as they arrive;
/// 6. on expiry, `session/cancel` and then the kill — with the frames collected so far kept, and an
///    exit that says marion ended it.
pub fn run_acp_child(spec: AcpChildSpec<'_>) -> Result<AcpRun, AcpChildError> {
    // Before the spawn: a declaration marion cannot correlate against is a hang, and a hang after a
    // process exists costs the whole wall clock to discover.
    let session_new = match spec.session_declaration {
        Some(d) => d,
        None => {
            acp::session_new_request(marion_harness::adapter::SESSION_NEW_ID, &spec.inv.cwd, &[])
        }
    };
    let session_new_id = declared_id(&session_new)?;

    // `checked_add`, for `run_bounded`'s reason: `Instant + Duration` panics on overflow, and every
    // escape from here on abandons a live process. Saturating to the bound's own ceiling keeps an
    // unrepresentable request finite instead of turning it into "kill it now" or "never".
    let deadline = Instant::now()
        .checked_add(spec.bound)
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));

    let mut agent = Driver::spawn(spec.inv)?;
    (spec.on_started)(agent.pid);

    agent.send(&acp::initialize_request(INITIALIZE_ID), "initialize")?;
    let handshake = match agent.settle(INITIALIZE_ID, clip(deadline, HANDSHAKE_BUDGET)) {
        Some(f) => f,
        None => {
            return Err(agent.refuse(|end, frames| AcpChildError::NoHandshake {
                waited: HANDSHAKE_BUDGET,
                frames,
                stderr: excerpt(&end.stderr),
            }));
        }
    };
    // Parsed, not merely received. §3.3's stage two is what narrows this agent's capabilities, and
    // an agent that answers with a wire version marion does not speak has to be refused here rather
    // than prompted anyway and read with a v1 reader.
    if let Err(e) = AgentHandshake::parse(&handshake) {
        let _ = agent.finish(true);
        return Err(AcpChildError::Handshake(e));
    }

    agent.send(&session_new, "session/new")?;
    let opened = match agent.settle(session_new_id, clip(deadline, SESSION_BUDGET)) {
        Some(f) => f,
        None => {
            return Err(agent.refuse(|end, frames| AcpChildError::NoSession {
                waited: SESSION_BUDGET,
                frames,
                stderr: excerpt(&end.stderr),
            }));
        }
    };
    let session = match acp::session_id(&opened) {
        Ok(s) => s,
        Err(e) => {
            let _ = agent.finish(true);
            return Err(AcpChildError::SessionRefused(e));
        }
    };

    agent.send(
        &acp::prompt_request(PROMPT_ID, &session, spec.prompt),
        "session/prompt",
    )?;
    let answered = agent.settle(PROMPT_ID, deadline).is_some();
    if !answered {
        // §8's interrupt step in ACP's own vocabulary, and it runs *before* the signal because it
        // is the cleaner one: the pending prompt answers a cancel with `stopReason: "cancelled"`,
        // which is a frame in the transcript, where a SIGKILL is a hole in it. Written
        // best-effort — an agent whose stdin has already closed is one the kill below handles.
        let _ = agent.write(&acp::cancel_notification(&session));
        agent.settle(PROMPT_ID, Instant::now() + CANCEL_GRACE);
    }

    let (end, _) = agent.finish(!answered);
    Ok(AcpRun {
        stdout: end.stdout,
        stderr: end.stderr,
        exit: end.exit,
        capture_truncated: end.capture_truncated,
    })
}

/// The id the adapter stamped on its `session/new`, or a refusal naming what arrived instead.
fn declared_id(request: &Value) -> Result<u64, AcpChildError> {
    let method = request.get("method").and_then(Value::as_str);
    if method != Some("session/new") {
        return Err(AcpChildError::Declaration {
            what: match method {
                Some(m) => format!("its method is `{m}`"),
                None => "it names no method at all".into(),
            },
        });
    }
    request
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| AcpChildError::Declaration {
            what: "it carries no numeric `id`, so the agent's answer could not be told from a \
                   notification"
                .into(),
        })
}

/// A step's deadline: its own measured budget, clipped to what is left of the caller's wall clock.
/// Never the later of the two — the caller's number is a ceiling, and a step budget that outran it
/// would make the wall clock advisory.
fn clip(overall: Instant, budget: Duration) -> Instant {
    let step = Instant::now()
        .checked_add(budget)
        .unwrap_or_else(|| Instant::now() + budget / 2);
    step.min(overall)
}

/// The last of `s`, bounded. The *last*, because a process that failed at startup says why in its
/// final lines and buries them under whatever it logged on the way there.
fn excerpt(s: &str) -> String {
    let s = s.trim_end();
    if s.len() <= STDERR_EXCERPT {
        return if s.is_empty() {
            "<empty>".into()
        } else {
            s.into()
        };
    }
    let cut = s.len() - STDERR_EXCERPT;
    let cut = (cut..s.len())
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(s.len());
    format!("…{}", &s[cut..])
}

/// What the shutdown produced.
struct Finish {
    stdout: String,
    stderr: String,
    exit: ChildExit,
    capture_truncated: bool,
}

/// A spawned ACP agent with both pipes drained by threads, so a client-bound request can be
/// answered while frames are still arriving.
///
/// **Every exit from this struct kills the agent, and kills its group.** An ACP agent is a
/// long-lived stdio server that never closes stdout on its own — §8's leak check names exactly this
/// shape — and the agent's own children (marion's MCP bridge among them) inherit its pipes.
/// [`Drop`] is what makes that true on the paths that return an error; [`Driver::finish`] is the
/// one that also reports what the kill took.
struct Driver {
    child: Child,
    pid: i32,
    stdin: Option<ChildStdin>,
    /// Every line the agent has written, in order. Shared with the reader thread.
    frames: Arc<Mutex<Vec<String>>>,
    /// The reader thread reached EOF, i.e. every holder of the stdout write end let go. False at
    /// the end of a run means the transcript is a prefix.
    stdout_eof: Arc<AtomicBool>,
    stderr: Option<Drain>,
    /// How far into `frames` the classifier has read. Requests are answered once and responses are
    /// indexed once, however many times a wait loop wakes.
    cursor: usize,
    /// `(id, frame)` for every response the agent has sent. Kept because S21 measured
    /// `session/update` notifications interleaved with, and arriving *before*, the response they
    /// belong to — so "the next line" is not the answer to anything, and an answer that arrives
    /// while marion is waiting on an earlier id must still be there when marion asks for it.
    responses: Vec<(u64, String)>,
}

impl Driver {
    fn spawn(inv: &Invocation) -> Result<Self, AcpChildError> {
        let mut cmd = Command::new(&inv.program);
        cmd.args(&inv.args)
            .current_dir(&inv.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &inv.env {
            cmd.env(k, v);
        }
        // Its own group. The agent starts marion's MCP bridge from the `session/new` declaration
        // (S21 watched it happen), that bridge inherits both of the agent's pipes, and a `read` on
        // a pipe returns EOF only when *every* write end is closed. So killing the agent alone
        // leaves the drains wedged — `duplex` measured that deadlock — and killing the group is
        // what closes them. `kill_process_tree` refuses marion's own pgid, so without a group of
        // its own the sweep would have nothing it is allowed to address.
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|source| AcpChildError::Spawn {
            program: inv.program.clone(),
            source,
        })?;
        let pid = child.id() as i32;
        let stdin = child.stdin.take();
        let frames = Arc::new(Mutex::new(Vec::new()));
        let stdout_eof = Arc::new(AtomicBool::new(false));
        let out = child.stdout.take().expect("stdout was piped");
        let sink = Arc::clone(&frames);
        let eof = Arc::clone(&stdout_eof);
        // Detached, and it has to be: the join is what would deadlock if anything the agent started
        // outlives it, so the group kill above is this thread's exit condition rather than a join.
        // Line-oriented because ACP's transport is newline-delimited and the driver answers
        // requests mid-turn — a whole-pipe read has nothing to answer with until the pipe closes.
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                sink.lock().expect("frame sink").push(line);
            }
            eof.store(true, Ordering::Relaxed);
        });
        let stderr = Drain::start(child.stderr.take().expect("stderr was piped"));
        Ok(Self {
            child,
            pid,
            stdin,
            frames,
            stdout_eof,
            stderr: Some(stderr),
            cursor: 0,
            responses: Vec::new(),
        })
    }

    /// One frame, newline-terminated. The transport is newline-delimited, so a frame that contained
    /// one would be two frames; `serde_json`'s compact form never does.
    fn write(&mut self, frame: &Value) -> std::io::Result<()> {
        let Some(w) = self.stdin.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "marion has already closed the agent's stdin",
            ));
        };
        writeln!(w, "{frame}")?;
        w.flush()
    }

    /// [`Self::write`], with the failure named after the step that could not be sent.
    fn send(&mut self, frame: &Value, step: &'static str) -> Result<(), AcpChildError> {
        match self.write(frame) {
            Ok(()) => Ok(()),
            Err(source) => {
                let _ = self.finish(true);
                Err(AcpChildError::Unwritable { step, source })
            }
        }
    }

    /// Wait for the response carrying `id`, **answering every client-bound request that arrives in
    /// the meantime**. `None` is "not by the deadline".
    ///
    /// The answering is not a courtesy. ACP is bidirectional: the agent sends marion requests
    /// mid-turn, and a request marion never answers stops the turn dead — the agent is waiting on
    /// marion, marion is waiting on the agent, and the only thing that ends it is the wall clock.
    /// S21's probe grew `answer_agent_requests` for this; `doctor` has no equivalent only because
    /// its micro-prompt is chosen so nothing is ever asked.
    fn settle(&mut self, id: u64, deadline: Instant) -> Option<String> {
        let mut gone: Option<Instant> = None;
        loop {
            self.classify();
            if let Some((_, frame)) = self.responses.iter().find(|(k, _)| *k == id) {
                return Some(frame.clone());
            }
            // A dead agent will not answer, and waiting out a 60 s budget to discover that turns
            // "the agent crashed" into "the agent hung" — two findings §8 insists on telling apart.
            // The grace is because the process dying and its last frame arriving are not ordered:
            // the frame crosses a pipe and a mutex after the exit status is readable.
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
            std::thread::sleep(POLL);
        }
    }

    /// Read every frame the agent has written since the last call: index the responses, answer the
    /// requests, ignore the notifications.
    fn classify(&mut self) {
        let new: Vec<String> = {
            let seen = self.frames.lock().expect("frame sink");
            if seen.len() <= self.cursor {
                return;
            }
            let from = self.cursor;
            self.cursor = seen.len();
            seen[from..].to_vec()
        };
        for line in new {
            // A line that is not JSON is kept in the transcript verbatim and classified as nothing.
            // Agents write banners and warnings to stdout, and `acp::json_frames` already skips
            // them; inventing a reply to one would be worse than ignoring it.
            let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let has_id = frame.get("id").is_some();
            if has_id && frame.get("method").is_some() {
                self.answer(&frame);
            } else if has_id
                && (frame.get("result").is_some() || frame.get("error").is_some())
                && let Some(k) = frame.get("id").and_then(Value::as_u64)
            {
                self.responses.push((k, line));
            }
        }
    }

    /// Answer one client-bound request.
    ///
    /// **Permissively, and the three kinds are not a policy marion invented here.** The shipped
    /// [`acp::initialize_request`] tells every agent that marion's client does `fs.readTextFile`,
    /// `fs.writeTextFile` and `terminal` before this driver sees a frame, so a refusal at this
    /// point would be a capability advertised and then withheld. And a denial would constrain
    /// nothing that is not already unconstrained: `AcpAdapter::compiled_permissions` records
    /// [`acp::NO_TOOL_AVAILABILITY_SURFACE`] because ACP has no field anywhere that narrows an
    /// agent's own tools, and S21's session had `write`, `edit` and `bash` in scope with marion
    /// asking for nothing. Denying the prompt while the agent holds `bash` would buy a slower turn
    /// and no safety.
    ///
    /// Anything else gets a JSON-RPC `-32601` naming the method — **answered, not dropped**, for
    /// `duplex`'s reason about unimplemented `control_request`s. `terminal/*` is the live case:
    /// marion advertises `terminal: true` and implements none of it, so a terminal-using agent
    /// learns that in one frame instead of hanging until the wall clock.
    fn answer(&mut self, request: &Value) {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").unwrap_or(&Value::Null).clone();
        let reply = match method {
            "session/request_permission" => match allow_option(&params) {
                Some(pick) => ok(
                    &id,
                    json!({"outcome": {"outcome": "selected", "optionId": pick}}),
                ),
                // Selecting an option that was not offered would be a fabricated answer; ACP's own
                // shape for "no option was taken" is an outcome, not an error, and it is the one
                // that lets the agent proceed.
                None => ok(&id, json!({"outcome": {"outcome": "cancelled"}})),
            },
            "fs/read_text_file" => match params.get("path").and_then(Value::as_str) {
                Some(p) => match std::fs::read_to_string(p) {
                    Ok(text) => ok(&id, json!({"content": text})),
                    Err(e) => err(&id, -32000, format!("{p}: {e}")),
                },
                None => err(&id, -32602, "fs/read_text_file names no `path`".into()),
            },
            // Performed, not acknowledged. S21's probe answered `{}` without writing, which is fine
            // for a probe and a lie to a node: an agent told its write succeeded goes on to read
            // the file back, or to report work it did not do.
            "fs/write_text_file" => match (
                params.get("path").and_then(Value::as_str),
                params.get("content").and_then(Value::as_str),
            ) {
                (Some(p), Some(c)) => match std::fs::write(p, c) {
                    Ok(()) => ok(&id, json!({})),
                    Err(e) => err(&id, -32000, format!("{p}: {e}")),
                },
                _ => err(
                    &id,
                    -32602,
                    "fs/write_text_file needs both `path` and `content`".into(),
                ),
            },
            other => err(
                &id,
                -32601,
                format!(
                    "marion's ACP client implements `session/request_permission`, \
                     `fs/read_text_file` and `fs/write_text_file`, and not `{other}`"
                ),
            ),
        };
        // Best-effort: an agent whose stdin has closed is being shut down anyway, and a write
        // failure here must not take down a turn whose frames are already worth reading.
        let _ = self.write(&reply);
    }

    /// Shut the agent down and collect everything.
    ///
    /// `marion_cut_it_short` is the caller's own statement that the turn did not finish, and it is
    /// what §6.7 calls an attributed kill — not something read off a signal number.
    fn finish(&mut self, marion_cut_it_short: bool) -> (Finish, usize) {
        // EOF on the agent's stdin first: it is the polite end of a stdio session, and an agent
        // that honours it exits before the signal.
        self.stdin.take();
        let interruptible = matches!(self.child.try_wait(), Ok(None));
        if interruptible {
            unsafe { kill(self.pid, SIGINT) };
        }
        let status = wait_bounded(&mut self.child, INTERRUPT_GRACE);
        let ended_on = if status.is_some() { SIGINT } else { SIGKILL };
        if status.is_none() {
            kill_process_tree(self.pid);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // Unconditional, and it is what makes the drains below terminate. The agent is reaped by
        // here; what is not is whatever it started — marion's own MCP bridge, holding the write end
        // of both pipes. The group survives its leader, so `kill(-pgid)` still addresses them.
        kill_process_tree(self.pid);

        let drain_deadline = Instant::now() + DRAIN_GRACE;
        while !self.stdout_eof.load(Ordering::Relaxed) && Instant::now() < drain_deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let stdout_complete = self.stdout_eof.load(Ordering::Relaxed);
        let collected = self.frames.lock().expect("frame sink").clone();
        let (stderr, stderr_complete) = match self.stderr.take() {
            Some(d) => d.finish(drain_deadline),
            None => (Vec::new(), true),
        };

        // **Marion's kill, reported as marion's kill.** On the short path the agent's own status is
        // not the turn's: an agent that raced the cancel and exited 0 would otherwise hand a
        // downstream reader an exit 0 for a turn marion cut off. `timed_out` is the field that says
        // what happened, and the code is dropped rather than reported as a success nobody earned.
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
        (
            Finish {
                stdout: collected.join("\n"),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
                exit,
                capture_truncated: !(stdout_complete && stderr_complete),
            },
            collected.len(),
        )
    }

    /// Shut down and build a refusal out of what the agent left behind. The agent's stderr is the
    /// only thing an operator can act on when the frames say nothing, so no error path may skip it.
    fn refuse(&mut self, make: impl FnOnce(Finish, usize) -> AcpChildError) -> AcpChildError {
        let (end, frames) = self.finish(true);
        make(end, frames)
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            unsafe { kill(self.pid, SIGKILL) };
            let _ = self.child.wait();
        }
        // The group, for the reason `finish` sweeps it: the agent's children hold its pipes, and a
        // driver dropped on an error path has the same obligation as one that returned a
        // transcript.
        kill_process_tree(self.pid);
    }
}

/// The option an agent's permission request should be answered with: the first one it labels as an
/// allow, else the first one it offered at all. Keyed on `kind` rather than on `optionId`, because
/// `kind` is ACP's own enumerated field (`allow_once`, `allow_always`, `reject_once`, …) and the id
/// is the agent's private string.
fn allow_option(params: &Value) -> Option<String> {
    let options = params.get("options")?.as_array()?;
    options
        .iter()
        .find(|o| {
            o.get("kind")
                .and_then(Value::as_str)
                .is_some_and(|k| k.starts_with("allow"))
        })
        .or_else(|| options.first())
        .and_then(|o| o.get("optionId")?.as_str())
        .map(str::to_string)
}

fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err(id: &Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// `Child::wait` with a ceiling. `None` is "still running at the deadline", which is a finding and
/// not an error.
fn wait_bounded(child: &mut Child, budget: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return Some(s),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_testsupport::scratch;
    use std::path::Path;

    /// A fake ACP agent, in `/bin/sh` — `duplex`'s technique, for its reason: the thing under test
    /// is a driver of a *process over pipes*, and a fake that is a Rust function tests the loop
    /// with the pipes taken out of it.
    ///
    /// Every script here carries its own self-destruct, so a regression that would hang **fails**
    /// instead of wedging the suite.
    fn agent(cwd: &Path, script: &str) -> Invocation {
        Invocation {
            program: "sh".into(),
            args: vec!["-c".into(), format!("( sleep 20; kill -9 $$ ) &\n{script}")],
            env: vec![],
            cwd: cwd.to_path_buf(),
            model: None,
        }
    }

    /// The `initialize` result, in the shape `AgentHandshake::parse` accepts — wire v1 and a named
    /// agent, since anything else is refused before a session is opened.
    const HELLO: &str = r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentInfo":{"name":"fake-acp","version":"0.1"},"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"fork":{}}}}}"#;
    const OPENED: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"ses_fake"}}"#;

    fn spec<'a>(
        inv: &'a Invocation,
        bound: Duration,
        on_started: &'a dyn Fn(i32),
    ) -> AcpChildSpec<'a> {
        AcpChildSpec {
            inv,
            session_declaration: None,
            prompt: "call marion's report tool",
            bound,
            on_started,
        }
    }

    /// **A whole turn, end to end**, including the two things a probe never does: a client-bound
    /// request answered mid-turn, and a transcript that the shipped ACP reader parses into the call
    /// the model made.
    #[test]
    fn a_driven_turn_answers_the_agents_own_request_and_yields_a_transcript_marion_can_read() {
        let dir = scratch("acp-turn");
        let perm = dir.join("permission-answer.json");
        let prompt_seen = dir.join("prompt.json");
        // Handshake, session, then: ask marion for permission and **block on the answer**, which is
        // the hang this driver exists to prevent. Only after it arrives does the turn produce the
        // two frames that make up a marion tool call, and its `stopReason`.
        let script = format!(
            r#"read init
printf '%s\n' '{HELLO}'
read new
printf '%s\n' '{OPENED}'
read prompt
printf '%s\n' "$prompt" > '{prompt}'
printf '%s\n' '{{"jsonrpc":"2.0","id":91,"method":"session/request_permission","params":{{"sessionId":"ses_fake","options":[{{"optionId":"no","kind":"reject_once"}},{{"optionId":"yes-always","kind":"allow_always"}}]}}}}'
read answer
printf '%s\n' "$answer" > '{perm}'
printf '%s\n' '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"ses_fake","update":{{"sessionUpdate":"tool_call","toolCallId":"c1","title":"marion_report","status":"pending","rawInput":{{}}}}}}}}'
printf '%s\n' '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"ses_fake","update":{{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed","rawInput":{{"narrative":"hello from acp"}}}}}}}}'
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"stopReason":"end_turn"}}}}'
# A long-lived stdio server: it does not exit when the turn ends, and marion must kill it.
sleep 15"#,
            prompt = prompt_seen.display(),
            perm = perm.display(),
        );
        let inv = agent(&dir, &script);
        let run = run_acp_child(spec(&inv, Duration::from_secs(20), &|_| {}))
            .expect("the fake agent completes a turn");

        // The prompt marion sent is ACP's own shape, and reached the agent.
        let sent: Value =
            serde_json::from_str(&std::fs::read_to_string(&prompt_seen).unwrap()).unwrap();
        assert_eq!(sent["method"], "session/prompt");
        assert_eq!(sent["params"]["sessionId"], "ses_fake");
        assert_eq!(
            sent["params"]["prompt"][0]["text"],
            "call marion's report tool"
        );

        // The agent asked, and marion answered — with the option the agent labelled as an allow,
        // not with the first one in the list.
        let answered: Value = serde_json::from_str(&std::fs::read_to_string(&perm).unwrap())
            .expect("marion answered the permission request");
        assert_eq!(answered["id"], 91);
        assert_eq!(answered["result"]["outcome"]["optionId"], "yes-always");

        // **The transcript is the agent's own frames and only those.** marion's requests are not in
        // it, which is the shape `tests/fixtures/s21/opencode-acp-session.jsonl` has.
        assert!(
            !run.stdout.contains(r#""method":"session/prompt""#),
            "marion's own writes must not be in the agent's transcript: {}",
            run.stdout
        );
        assert_eq!(
            run.stdout.lines().count(),
            6,
            "the initialize and session/new results, the permission ask, two updates and the \
             prompt result — and nothing of marion's: {}",
            run.stdout
        );

        // And it is what the shipped reader reads: the verb, its outcome and its arguments.
        let spelling = acp::ToolSpelling::ServerUnderscoreTool;
        assert_eq!(
            acp::marion_calls(&run.stdout, spelling),
            vec![marion_harness::MarionCall {
                verb: "report".into(),
                outcome: marion_harness::CallOutcome::Answered,
            }]
        );
        let out = acp::parse_stream(&run.stdout, run.exit, spelling);
        assert_eq!(out.narrative.as_deref(), Some("hello from acp"));
        assert_eq!(out.failure, None);

        assert!(!run.exit.timed_out, "the turn answered inside its bound");
        // **Not truncated**, and that is a claim about the kill and not about the fake: the script
        // leaves a `sleep` holding the stdout write end after `sh` dies, so the capture only
        // completes because the whole process group is swept.
        assert!(
            !run.capture_truncated,
            "a pipe was still held open: the group sweep did not reach the agent's children"
        );
    }

    /// **The wall clock, and a timeout that is not a lie.** The agent takes the prompt and never
    /// answers it. What must survive is everything it *did* say, plus its own `cancelled` — and the
    /// exit must say marion ended this, never exit 0.
    #[test]
    fn an_expired_turn_keeps_its_frames_and_reports_marions_own_kill() {
        let dir = scratch("acp-expiry");
        let script = format!(
            r#"read init
printf '%s\n' '{HELLO}'
read new
printf '%s\n' '{OPENED}'
read prompt
printf '%s\n' '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"ses_fake","update":{{"sessionUpdate":"agent_thought_chunk","content":{{"type":"text","text":"thinking"}}}}}}}}'
# The turn never answers. marion's `session/cancel` is what this reads, and ACP's own answer to a
# cancel is the pending prompt returning `cancelled`.
read cancel
printf '%s\n' "$cancel" > '{seen}'
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"stopReason":"cancelled"}}}}'
sleep 15"#,
            seen = dir.join("cancel.json").display(),
        );
        let inv = agent(&dir, &script);
        let started = Instant::now();
        let run = run_acp_child(spec(&inv, Duration::from_secs(2), &|_| {}))
            .expect("an expired turn is a transcript, not an error");

        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the bound was honoured"
        );
        // The cancel marion sent is ACP's notification shape: no id, or the agent would owe it an
        // answer instead of ending the prompt.
        let cancel: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("cancel.json")).unwrap())
                .expect("marion sent session/cancel on expiry");
        assert_eq!(cancel["method"], "session/cancel");
        assert!(cancel.get("id").is_none());

        // Everything the agent said before the expiry is still here, and so is its own last word.
        assert!(run.stdout.contains("agent_thought_chunk"), "{}", run.stdout);
        assert!(
            run.stdout.contains(r#""stopReason":"cancelled""#),
            "{}",
            run.stdout
        );

        // **Never an exit 0.** `timed_out` is marion's attributed kill (§6.7) and the code is
        // dropped rather than reported as a success the turn did not reach.
        assert!(run.exit.timed_out);
        assert_eq!(run.exit.code, None);
        assert!(run.exit.signal.is_some());
    }

    /// **An agent that never answers `initialize` is refused by name**, with its own stderr
    /// attached — which is the only thing an operator can act on when there are no frames. S22's
    /// zero-frame `npx` probe is this case, and it was a cold download rather than a broken agent.
    #[test]
    fn an_agent_that_never_answers_initialize_is_refused_by_name() {
        let dir = scratch("acp-mute");
        // Reads marion's `initialize`, says nothing on stdout, complains on stderr and dies.
        let inv = agent(
            &dir,
            "read init\necho 'fake-acp: this build has no ACP support' >&2\nexit 3",
        );
        let started = Instant::now();
        let e = run_acp_child(spec(&inv, Duration::from_secs(20), &|_| {}))
            .expect_err("no handshake, no run");
        match &e {
            AcpChildError::NoHandshake { frames, stderr, .. } => {
                assert_eq!(*frames, 0);
                assert!(stderr.contains("no ACP support"), "{stderr}");
            }
            other => panic!("expected NoHandshake, got {other:?}"),
        }
        assert!(e.to_string().contains("initialize"), "{e}");
        // And a dead agent is not waited out: the handshake budget is 30 s and this must not spend
        // it, or "the agent crashed" and "the agent hung" become the same finding.
        assert!(
            started.elapsed() < HANDSHAKE_BUDGET / 2,
            "waited {:?} on an agent that had already exited",
            started.elapsed()
        );
    }

    /// **§6.1 step 7's ordering: the pid is confirmed before one byte reaches the node.** The
    /// callback here does what the real one does — a durable write — and the agent records whether
    /// that write had landed by the time marion's first frame arrived.
    #[test]
    fn the_pid_is_confirmed_before_the_first_byte_is_written_to_the_agent() {
        let dir = scratch("acp-order");
        let marker = dir.join("started");
        let order = dir.join("order");
        let script = format!(
            r#"read init
if [ -f '{marker}' ]; then printf 'confirmed-first\n' > '{order}'; else printf 'wrote-first\n' > '{order}'; fi
printf '%s\n' '{HELLO}'
read new
printf '%s\n' '{OPENED}'
read prompt
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"stopReason":"end_turn"}}}}'
sleep 15"#,
            marker = marker.display(),
            order = order.display(),
        );
        let inv = agent(&dir, &script);
        let seen = Mutex::new(Vec::new());
        let on_started = |pid: i32| {
            seen.lock().unwrap().push(pid);
            // A journal append and an fsync take real time, and the ordering must hold because the
            // call happens first — not because it happens to be fast.
            std::thread::sleep(Duration::from_millis(300));
            std::fs::write(&marker, pid.to_string()).unwrap();
        };
        let run = run_acp_child(spec(&inv, Duration::from_secs(20), &on_started)).expect("a turn");

        assert_eq!(
            std::fs::read_to_string(&order).unwrap().trim(),
            "confirmed-first",
            "the agent had already been handed marion's `initialize` when the pid was confirmed"
        );
        let pids = seen.into_inner().unwrap();
        assert_eq!(pids.len(), 1, "confirmed once, not once per frame");
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            pids[0].to_string(),
            "the pid handed to the caller is the one that was spawned"
        );
        assert!(pids[0] > 1);
        assert!(run.stdout.contains("end_turn"));
    }

    /// A declaration that is not a `session/new` request is refused **before anything is spawned**.
    /// The id is read out of the adapter's own frame, so a driver and an adapter that disagreed
    /// about it would hang rather than fail — this is what keeps that from being possible.
    #[test]
    fn a_declaration_marion_cannot_correlate_against_is_refused_before_a_process_exists() {
        let dir = scratch("acp-decl");
        // A program that does not exist: reaching the spawn at all is the failure being excluded.
        let inv = Invocation {
            program: "/nonexistent/acp-agent".into(),
            args: vec![],
            env: vec![],
            cwd: dir.to_path_buf(),
            model: None,
        };
        let refuse = |d: Value| {
            run_acp_child(AcpChildSpec {
                inv: &inv,
                session_declaration: Some(d),
                prompt: "hi",
                bound: Duration::from_secs(5),
                on_started: &|_| panic!("nothing may be spawned"),
            })
            .expect_err("refused")
        };
        assert!(matches!(
            refuse(json!({"jsonrpc": "2.0", "id": 1, "method": "session/prompt"})),
            AcpChildError::Declaration { .. }
        ));
        let anonymous = refuse(json!({"jsonrpc": "2.0", "method": "session/new", "params": {}}));
        assert!(
            matches!(&anonymous, AcpChildError::Declaration { what } if what.contains("`id`")),
            "{anonymous}"
        );
        // The real one passes this gate and fails at the spawn, which is the next thing that can go
        // wrong and proves the gate is not refusing everything.
        let real = acp::session_new_request(marion_harness::adapter::SESSION_NEW_ID, &dir, &[]);
        assert!(matches!(refuse(real), AcpChildError::Spawn { .. }));
    }

    /// The permission answer is chosen by ACP's `kind`, not by position: an agent that lists its
    /// rejection first must not be answered with it.
    #[test]
    fn the_permission_option_is_chosen_by_its_kind_and_a_request_with_none_is_still_answered() {
        let offered = json!({"options": [
            {"optionId": "n", "kind": "reject_once"},
            {"optionId": "y", "kind": "allow_once"},
        ]});
        assert_eq!(allow_option(&offered).as_deref(), Some("y"));
        // No allow on offer: the agent's own first option, rather than an id marion invented.
        assert_eq!(
            allow_option(&json!({"options": [{"optionId": "only", "kind": "reject_always"}]}))
                .as_deref(),
            Some("only")
        );
        assert_eq!(allow_option(&json!({"options": []})), None);
        assert_eq!(allow_option(&Value::Null), None);
    }
}
