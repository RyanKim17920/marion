//! The **typed control plane**, shared by the root and by a child (design §6.1 step 8, §3.4, §5.2).
//!
//! Everything here is the protocol marion speaks to a node whose `ExecutionSurfaces` declare
//! `ControlTransport::Typed(_)`: pipes rather than a pty, a readiness marker, an
//! `initialize` `control_request`/`control_response` round trip, a user frame written **after**
//! both, a frame loop, and a `can_use_tool` answer with nobody to ask.
//!
//! # Why it is one module and not two copies
//!
//! §6.1 step 8's gate is normative for *any* launcher driving Claude Code headlessly — it says so
//! in as many words — so it cannot be a property of the root's launch path. Until this module
//! existed it was: `root::launch_duplex` held the only implementation, and `run_spawn` drove every
//! child as if it were `LaunchOnly`, which is exactly why a `claude` child took turn one with
//! `"tools":[]` and `"mcp_servers":[{"name":"marion","status":"pending"}]`, got the session-title
//! stub back, and exited **0 having called nothing**. Writing the gate a second time inside
//! `run_spawn` would have been the drift §9 warns about — the same warning under which `marion run`
//! and the bridge share `run_spawn` *"by linkage — that is what stops the two paths drifting while
//! no socket exists to force agreement."*
//!
//! # What a root and a child do **not** share
//!
//! Only three things, and none of them is protocol:
//!
//! - a child **has a `TaskContract`** and may `report`; a root has neither (§9). That is an
//!   allowlist and a contract-assembly difference, both of which live at the call sites.
//! - a child is bounded by its own `timeout_secs` — a **wall clock**, enforced here by
//!   [`DuplexSpec::wall_clock`] and [`crate::run::kill_process_tree`], the one kill in the
//!   workspace measured to leave no survivors (S7). A root is offered no wall clock at all, so it
//!   passes `None` and no process group is created for it.
//! - the `Blocked` budget a permission ask consumes before it is denied differs, so it is a field
//!   rather than a constant.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command as SysCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant};

use marion_harness::{ControlTransport, ExecutionSurfaces};
use serde_json::{Value, json};

use crate::run::{DRAIN_GRACE, Drain, kill_process_tree};

/// Which launch path a node takes, derived from its adapter's [`ExecutionSurfaces`].
///
/// §3.4: *"branch code on `ExecutionSurfaces`"*, never on a harness name. **One derivation for the
/// root and the child both** — `root::root_path` and `run_spawn` are the same function under two
/// names, so a fifth harness gets the right path on *both* axes by declaring its surfaces and
/// changing nothing in either module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchPath {
    /// A typed control plane: prompt, steer, interrupt — Claude Code's `stream-json` pipe pair.
    /// The prompt is a frame written after the readiness gate, never an argv element.
    Duplex,
    /// The prompt rides argv and there is no channel afterwards. `codex exec --json` is this, and
    /// so are gemini and opencode.
    LaunchOnly,
}

/// The path a node with these surfaces takes.
///
/// A `TerminalInput` node has no answer here and is refused rather than forced down one of the
/// two: marion **MUST NOT** give a headless node a pty on stdin (§5.2), and typing a prompt into a
/// pty is a third path nobody has written.
pub fn launch_path(surfaces: &ExecutionSurfaces) -> Option<LaunchPath> {
    match surfaces.control {
        ControlTransport::Typed(_) => Some(LaunchPath::Duplex),
        ControlTransport::LaunchOnly => Some(LaunchPath::LaunchOnly),
        ControlTransport::TerminalInput => None,
    }
}

/// One `stream-json` user turn, in the shape `tests/fixtures/s1/stdin.jsonl` records.
pub fn user_message(prompt: &str) -> String {
    json!({
        "type": "user",
        "session_id": "",
        "message": {"role": "user", "content": [{"type": "text", "text": prompt}]},
        "parent_tool_use_id": null,
    })
    .to_string()
}

/// The `initialize` control request. §5.2: optional, `request_id` client-generated.
pub fn initialize_request(request_id: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {"subtype": "initialize", "hooks": {}},
    })
    .to_string()
}

/// Is this frame the `control_response` to `request_id`?
pub fn is_control_response_to(frame: &Value, request_id: &str) -> bool {
    frame.get("type").and_then(Value::as_str) == Some("control_response")
        && frame
            .pointer("/response/request_id")
            .and_then(Value::as_str)
            == Some(request_id)
}

/// The `request_id` and `tool_name` of an inbound `can_use_tool` request, if that is what this
/// frame is.
///
/// **Measured, not decompiled** — S9, `tests/fixtures/s9/`. Three fields and only three are common
/// to every ask 2.1.220 emits: the **top-level** `request_id`, `request.subtype`, and
/// `request.tool_name`. Everything else varies with the tool kind — an MCP verb carries
/// `display_name`, `input` and one `permission_suggestions` entry; a built-in `Bash` call adds
/// `description` and `blocked_path` and suggests three. So this reads the three and no more:
/// requiring `blocked_path` would work against Bash and fail against `mcp__marion__*`.
pub fn can_use_tool_request(frame: &Value) -> Option<(String, String)> {
    if frame.get("type").and_then(Value::as_str) != Some("control_request") {
        return None;
    }
    if frame.pointer("/request/subtype").and_then(Value::as_str) != Some("can_use_tool") {
        return None;
    }
    let request_id = frame.get("request_id").and_then(Value::as_str)?.to_string();
    let tool = frame
        .pointer("/request/tool_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some((request_id, tool))
}

/// The `request_id` and `request.subtype` of an inbound `control_request` marion has **no
/// implementation for**, if that is what this frame is. `can_use_tool` — the one kind marion does
/// answer — is deliberately excluded, so the two arms of the frame loop cannot both fire.
///
/// §5.2: the CLI emits `can_use_tool`, **hook callbacks** and **`request_user_dialog`** as
/// `control_request` frames on the same stdout stream, *"expecting a `control_response`"*. Only the
/// first is measured (S9). This reads the two fields §5.2 makes normative for the whole inbound
/// direction — the **top-level** `request_id` and `request.subtype` — because they are the only
/// ones a kind marion has never seen can be assumed to carry. A frame with no top-level
/// `request_id` is not answerable at all (§5.2: *"a frame without one could be neither answered nor
/// cancelled"*), so it yields `None` and is left on the transcript.
pub fn unanswerable_control_request(frame: &Value) -> Option<(String, String)> {
    if frame.get("type").and_then(Value::as_str) != Some("control_request") {
        return None;
    }
    let subtype = frame
        .pointer("/request/subtype")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if subtype == "can_use_tool" {
        return None;
    }
    let request_id = frame.get("request_id").and_then(Value::as_str)?.to_string();
    Some((request_id, subtype.to_string()))
}

/// marion's answer to an inbound `control_request` it does not implement — **an answer, because
/// silence is a deadlock.**
///
/// Until this existed the frame loop matched `can_use_tool` and nothing else, so any other inbound
/// `control_request` was pushed onto the transcript and never answered. A child would eventually
/// die on its contract's wall clock as `TimedOut` with no record of why; a **root** is launched
/// with `wall_clock: None` (§9 offers a root no ceiling) and would wait **forever** — `blocked_bound`
/// is a `sleep` inside the `can_use_tool` arm, not a watchdog.
///
/// **Why `subtype: "error"` and not a `"success"` carrying a refusal.** S9 measured that the
/// envelope's own `subtype` reports *whether an answer was produced*, not what the answer was — the
/// verdict lives in `response.response`. That is precisely why `success` is wrong here: marion has
/// produced no answer, and the `response` payload a `success` must carry is **subtype-specific** —
/// `{"still_queued":[]}` for `interrupt`, `{"behavior":…}` for `can_use_tool`, ~30 kB of session
/// catalogue for `initialize` (all §5.2, all measured). There is no generic success payload; sending
/// `{"behavior":"deny"}` in reply to `request_user_dialog` would be marion inventing a body for a
/// protocol it has never observed, and claiming to have answered while doing it. An error carries a
/// string, which is shape-free, and it is the one reply a requester must already be prepared for,
/// since any request it makes can fail. It is also **loud**: the far side learns marion cannot serve
/// this kind, rather than inferring it from a tool that silently does not work.
///
/// **Neither the request nor this reply is measured — §11 item 14.** Hook callbacks and
/// `request_user_dialog` are still designed-on-decompilation: no capture in this repo contains
/// either, so a faithful implementation would need fixtures that do not exist, and this is
/// deliberately *not* one. It is the narrower claim that marion must not deadlock on a frame it
/// cannot serve. (`initialize_request` sends `"hooks": {}`, so hook callbacks should not fire today
/// — but the whole point is not to depend on that.)
pub fn unsupported_request_response(request_id: &str, subtype: &str) -> String {
    let subtype = match subtype {
        "" => "<no request.subtype>",
        s => s,
    };
    json!({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id,
            "error": format!(
                "marion does not implement the `{subtype}` control_request (design §11 item 14: \
                 unmeasured, no fixture exists). Answered rather than dropped, so the session is \
                 not left waiting for a control_response that would never arrive."
            ),
        },
    })
    .to_string()
}

/// What marion tells a node it denied because there was **nobody to ask** (§9), after holding the
/// ask for the node's whole `Blocked` budget.
///
/// A constant because it is now one of two answers rather than the only one: see
/// [`decided_permission`] for the asks that never reach it, and why saying this about one of them
/// would be a false diagnosis rather than merely a slow one.
pub const NO_ANSWERER: &str =
    "marion: no permission answerer in M1; the node's Blocked bound expired";

/// **The ask marion can answer itself, without an answerer and without waiting.**
///
/// §9's block-then-deny rule is written for a permission marion *has nobody to ask about*: it holds
/// the node's `Blocked` budget precisely because someone might, in principle, arrive to answer. An
/// ask whose answer is fixed by §5.4 is not that: a root's `report` is refused by a rule, and no
/// amount of waiting changes it. Holding one cost a real root **900 s** (`bin/marion.rs`'s
/// `blocked_bound_secs` default) and then blamed a missing answerer for a decision marion had
/// already made — a wrong reason after a fifteen-minute stall.
///
/// **The spelling comes from the adapters and the rule comes from [`crate::bridge`]**, so this
/// function contains neither. `tool` arrives in the harness's own vocabulary
/// (`mcp__marion__report`, `marion_report`, …), and the mapping from marion's verb to that
/// spelling *is* §3.1's adapter contract — restating one here would keep passing after an adapter
/// changed it. Every adapter's spelling is checked rather than this node's, deliberately: §3.4
/// forbids branching on a harness name, and the question being asked ("does this frame name
/// marion's own `report`?") does not depend on which harness is asking.
fn decided_permission(depth: u32, tool: &str) -> Option<&'static str> {
    let is_report = marion_core::harness::Harness::ALL.iter().any(|h| {
        marion_harness::adapter_for(*h)
            .is_ok_and(|a| a.marion_tool_name(crate::bridge::REPORT) == tool)
    });
    is_report
        .then(|| crate::bridge::authorization_refusal(depth, crate::bridge::REPORT))
        .flatten()
}

/// marion's answer to a permission it has nobody to ask (§9).
///
/// M1 has no TUI, so a genuine permission request blocks until the node's per-episode `Blocked`
/// bound expires and is then **denied** — the node is not killed, because "expired" and
/// "terminated" are different events.
///
/// **Measured 2026-08-03 — S9, `tests/fixtures/s9/can-use-tool-deny.stdin.jsonl`.** This exact
/// string was written to a real 2.1.220's stdin and accepted: the CLI turned the denial into an
/// `is_error` `tool_result` carrying `message` verbatim, tagged it
/// `non_execution_kind: "permission-rule"`, listed the call under the run's `permission_denials`,
/// and **finished the turn normally** (`terminal_reason: "completed"`, exit 0).
/// `crates/marion-supervisor/tests/permission_round_trip.rs` re-runs it.
///
/// The corresponding allow is `{"behavior":"allow"}` with an **optional** `updatedInput`; S9
/// measured a bare allow running the tool with the model's original input. marion does not send
/// one in M1 — it has no permission answerer — so no function for it exists here.
pub fn deny_response(request_id: &str, reason: &str) -> String {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {"behavior": "deny", "message": reason},
        },
    })
    .to_string()
}

/// Wait for the bridge's readiness marker.
///
/// Polling a file rather than sleeping: §6.1 step 8 says a launcher **MUST NOT** substitute a sleep
/// for this, because a sleep encodes the very race it is covering and the failure it permits is
/// silent.
pub fn wait_for_ready(path: &Path, timeout: StdDuration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(StdDuration::from_millis(10));
    }
    path.exists()
}

/// Everything the driver observes on a node's stdout, handed to a [`DuplexSpec::sink`] **as it is
/// observed** rather than after the run returns.
///
/// Two variants and not one, because a stdout line that is not JSON is still something the node
/// said. `DuplexOutcome::stdout` keeps it verbatim either way, but a sink that only ever saw
/// `Frame` would render a run in which the node printed a stack trace as an unexplained silence —
/// the failure mode this module exists to stop.
#[derive(Debug, Clone, Copy)]
pub enum StreamEvent<'a> {
    /// One parsed `stream-json` frame, in the order it arrived.
    Frame(&'a Value),
    /// A stdout line that did not parse as JSON, verbatim and without its newline.
    Unparsed(&'a str),
}

/// Where a driver sends [`StreamEvent`]s while the node is still running.
///
/// **`Fn`, not `FnMut`, and borrowed rather than owned.** The driver is a single reader loop that
/// holds the spec by shared reference, so a sink that needs state carries its own `Cell`/`RefCell`
/// and a sink that needs none — the renderer in `bin/marion.rs` writes straight to a stream — costs
/// nothing. Nothing here is `Send`: the sink is called on the reader's own thread, between frames.
pub type StreamSink<'a> = &'a dyn Fn(StreamEvent<'_>);

/// What one duplex session needs, over and above the compiled [`marion_harness::Invocation`] the
/// caller has already turned into a `Command`.
#[derive(Clone)]
pub struct DuplexSpec<'a> {
    /// The marker the bridge touches **after flushing** its `tools/list` reply (§6.1 step 8).
    pub ready_file: &'a Path,
    /// The node's turn, written as a `user` frame once the gate has passed — never argv.
    pub prompt: &'a str,
    /// `request_id` for the `initialize` round trip. The node's own id, so one node's reply can
    /// never satisfy another's wait.
    pub init_id: String,
    /// How long the marker may take to appear before the run is refused.
    pub mcp_ready_timeout: StdDuration,
    /// §9's per-episode `Blocked` budget: how long an unanswerable permission ask is held before it
    /// is denied and the node allowed to proceed.
    pub blocked_bound: StdDuration,
    /// Where in the tree the node being driven sits, **root = 0** (§3.1/§6.1 step 2).
    ///
    /// The driver needs it for the same reason `main::caller_from` does: some asks have an answer
    /// that depends on nothing but which node is asking, and a driver that was not told cannot tell
    /// them from the ones marion genuinely has nobody to ask about. Passed rather than inferred from
    /// `wall_clock.is_none()` — that a root is the node with no wall clock is a fact about §9's
    /// budgets, not a definition of a root, and reading one off the other would silently mean
    /// something else the first time a bounded root exists.
    pub depth: u32,
    /// A wall-clock ceiling, after which the node's whole process group is killed. `Some` for a
    /// child, which is bounded by its contract's `timeout_secs`; `None` for a root, which marion
    /// offers no wall clock at all (§9).
    pub wall_clock: Option<StdDuration>,
    /// Where to send each [`StreamEvent`] the moment it is read, or `None` to observe the run only
    /// through the returned [`DuplexOutcome`].
    ///
    /// **This is why the driver contains no `println!`, and must not grow one.** Both callers reach
    /// this same function, and only one of them has a human on the other end:
    ///
    /// - `marion run` is a person at a terminal, who otherwise learns nothing until the whole run
    ///   returns. It passes a sink.
    /// - `run_spawn` drives a child from *inside* `marion-supervisor`, whose own stdout **is** the
    ///   stdio MCP stream the root harness is parsing. A byte written there that is not a JSON-RPC
    ///   message corrupts the protocol marion itself speaks. It passes `None`, and
    ///   `a_child_run_writes_not_one_byte_of_the_nodes_stream_to_marions_own_stdout` is the test
    ///   that keeps it true whatever a future frame handler decides to be helpful about.
    pub sink: Option<StreamSink<'a>>,
    /// Called **once**, with the child's pid, at the first instant a process exists — between
    /// `command.spawn()` and the first byte written to its stdin.
    ///
    /// §6.1 step 7's confirmation is the caller's to write, not the driver's, but only the driver
    /// knows the pid and only the driver knows when the process came into being. This is that
    /// seam, and the position is the whole of it: a hook called after the run would name a process
    /// its caller had already watched die, which is exactly the record item 28 step 1 replaces.
    ///
    /// `&dyn Fn` for [`StreamSink`]'s reason — the driver holds the spec by shared reference and a
    /// hook that needs state carries its own cell. Called on the driver's own thread, so a hook
    /// that blocks delays the node's first turn; the one production hook appends one journal
    /// record and fsyncs it, which is the cost §6.1 step 7 is written to pay.
    pub on_started: Option<&'a dyn Fn(i32)>,
}

/// Hand-written because a [`StreamSink`] is a `dyn Fn` and cannot derive it. The sink is reported as
/// present or absent, which is the only fact about it a debug print could honestly carry.
impl std::fmt::Debug for DuplexSpec<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DuplexSpec")
            .field("ready_file", &self.ready_file)
            .field("prompt", &self.prompt)
            .field("init_id", &self.init_id)
            .field("mcp_ready_timeout", &self.mcp_ready_timeout)
            .field("blocked_bound", &self.blocked_bound)
            .field("depth", &self.depth)
            .field("wall_clock", &self.wall_clock)
            .field("sink", &self.sink.map(|_| "<sink>"))
            .field("on_started", &self.on_started.map(|_| "<on_started>"))
            .finish()
    }
}

/// What a duplex session produced.
#[derive(Debug, Default)]
pub struct DuplexOutcome {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    /// Every frame the node emitted, parsed.
    pub transcript: Vec<Value>,
    /// The same stdout as text, verbatim and including any non-JSON line, so an adapter's
    /// `parse_stream` reads exactly what the node wrote rather than a re-serialisation of it.
    pub stdout: String,
    pub stderr: String,
    /// `tool_name` of each permission request marion denied on expiry of the `Blocked` bound.
    pub denied_permissions: Vec<String>,
    /// `request.subtype` of each inbound `control_request` marion had no implementation for and
    /// answered with an error rather than dropping. Empty on every run this repo has ever
    /// captured — §11 item 14's two unmeasured kinds are the only things that populate it — so a
    /// non-empty one is evidence a kind marion cannot serve is actually being emitted.
    pub unanswered_control_requests: Vec<String>,
    /// marion's own wall-clock bound expired and the node's process group was killed.
    pub timed_out: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DuplexError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// **The loud refusal §6.1 step 8 requires.** The alternative is a first turn that went out
    /// without marion's tools, ended as plain text, and exited 0 with nothing anywhere reporting it
    /// (§12).
    #[error(
        "the harness never fetched marion's tool list within {0:?}: no {1} appeared. The node \
         would have taken its first turn without marion's tools, so the run is refused rather than \
         allowed to end in plain text with no error anywhere."
    )]
    McpNeverReady(StdDuration, PathBuf),
    #[error("the node exited before answering marion's initialize control request")]
    DiedBeforeInitialize,
}

/// Start `command` as a duplex node and drive it to completion.
///
/// The order is §6.1 step 8's, and every step of it is load-bearing:
///
/// 1. spawn with all three streams piped — never a pty, which §5.2 forbids for a headless node;
/// 2. **wait for the bridge's marker**, which is the event "the harness has been sent marion's tool
///    list". Observed on marion's own side, because 2.1.220 emits no `system/init` until *after*
///    the first user frame and gating on it would deadlock;
/// 3. an `initialize` round trip, proving the harness's event loop has run since the tool list
///    reached it. Observing the bridge's side proves the tools were *sent*; only a completed round
///    trip proves anything was processed;
/// 4. **then** the prompt;
/// 5. frames until the terminal `result`, answering `can_use_tool` on the way — and answering
///    **every other** inbound `control_request` too, with an error naming the unimplemented kind,
///    because §5.2 says each of them expects a `control_response` and one that never arrives hangs
///    a root forever;
/// 6. `drop(stdin)`, which is what ends a `--input-format stream-json` session.
pub fn run_duplex(
    command: &mut SysCommand,
    spec: &DuplexSpec<'_>,
) -> Result<DuplexOutcome, DuplexError> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if spec.wall_clock.is_some() {
        // The group exists so marion's expiry kill can address the whole tree without touching its
        // own group — `signal_targets` refuses marion's own pgid, so without this the kill would
        // reach nothing. With no wall clock there is no kill and nothing to address, which is why
        // a root's launch is left byte-identical to what it was.
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let pid = child.id() as i32;
    // **Before one byte reaches the node.** §6.1 step 7's confirmation is the caller's to write and
    // this is the first instant it can be written truthfully; putting it here rather than after the
    // ready gate means the window in which a process exists and no durable record names it is one
    // append and one fsync wide, instead of the node's whole first turn. See
    // [`DuplexSpec::on_started`].
    if let Some(started) = spec.on_started {
        started(pid);
    }

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    // **Bounded, and that is load-bearing rather than tidy.** A `read_to_string` on this pipe
    // returns only when *every* holder of its write end has closed it — and the node's own MCP
    // bridge, which the harness started and marion did not, inherits that write end. So an
    // unconditional join makes marion's liveness rest on the bridge dying, and on the refusal path
    // below (kill the node, report the cause) it deadlocks outright: measured, the whole run wedged
    // in `read_to_string` after the node was killed. [`Drain`] is `run_bounded`'s answer to exactly
    // this, and it is reused rather than re-derived — one stoppable reader, so an abandoned drain
    // leaves neither a live thread nor a live fd behind.
    let stderr = Drain::start(child.stderr.take().expect("stderr was piped"));
    let mut lines = BufReader::new(stdout).lines();

    // The wall clock, if there is one. A watchdog rather than a bound on each read: the reads are
    // blocking and a node that hangs *between* frames must still be killed.
    let finished = Arc::new(AtomicBool::new(false));
    let expired = Arc::new(AtomicBool::new(false));
    let watchdog = spec.wall_clock.map(|bound| {
        let finished = Arc::clone(&finished);
        let expired = Arc::clone(&expired);
        std::thread::spawn(move || {
            let deadline = Instant::now() + bound;
            while Instant::now() < deadline {
                if finished.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(StdDuration::from_millis(20));
            }
            if finished.load(Ordering::Relaxed) {
                return;
            }
            expired.store(true, Ordering::Relaxed);
            kill_process_tree(pid);
        })
    });
    // Every exit from here on goes through this, so no path can leave the watchdog running or the
    // stderr drain wedged. `kill` is set on the paths that abandon a live node: killing the direct
    // child alone would leave its bridge holding the pipe, which is the deadlock described above.
    let stop = |kill: bool,
                watchdog: Option<std::thread::JoinHandle<()>>,
                child: &mut std::process::Child,
                stderr: Drain| {
        if kill {
            if spec.wall_clock.is_some() {
                // A group of our own exists, so the whole tree can be addressed (S7's two-step
                // kill). Without one, `signal_targets` would refuse marion's own pgid and the
                // sweep would reach nothing, so the direct kill below is all there is.
                kill_process_tree(pid);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        finished.store(true, Ordering::Relaxed);
        if let Some(h) = watchdog {
            let _ = h.join();
        }
        let (bytes, _complete) = stderr.finish(Instant::now() + DRAIN_GRACE);
        String::from_utf8_lossy(&bytes).into_owned()
    };

    let mut outcome = DuplexOutcome::default();

    if !wait_for_ready(spec.ready_file, spec.mcp_ready_timeout) {
        stop(true, watchdog, &mut child, stderr);
        return Err(DuplexError::McpNeverReady(
            spec.mcp_ready_timeout,
            spec.ready_file.to_path_buf(),
        ));
    }

    // One round trip through the harness's event loop, after the tool list was flushed to it.
    // Every stdout line the driver reads passes through here, which is the one place a live
    // observer can be fed without any handler downstream having to remember to. The sink is called
    // *before* the frame is dispatched or recorded, so what a watcher sees is the arrival order,
    // not marion's handling order — and a frame that makes a later step fail has still been shown.
    let record = |outcome: &mut DuplexOutcome, line: &str| -> Option<Value> {
        outcome.stdout.push_str(line);
        outcome.stdout.push('\n');
        let frame = serde_json::from_str::<Value>(line).ok();
        if let Some(sink) = spec.sink {
            match &frame {
                Some(v) => sink(StreamEvent::Frame(v)),
                None => sink(StreamEvent::Unparsed(line)),
            }
        }
        frame
    };
    writeln!(stdin, "{}", initialize_request(&spec.init_id))?;
    stdin.flush()?;
    let mut initialized = false;
    for line in lines.by_ref() {
        let line = line?;
        let Some(frame) = record(&mut outcome, &line) else {
            continue;
        };
        let done = is_control_response_to(&frame, &spec.init_id);
        outcome.transcript.push(frame);
        if done {
            initialized = true;
            break;
        }
    }
    if !initialized {
        stop(true, watchdog, &mut child, stderr);
        return Err(DuplexError::DiedBeforeInitialize);
    }

    writeln!(stdin, "{}", user_message(spec.prompt))?;
    stdin.flush()?;

    for line in lines.by_ref() {
        let line = line?;
        let Some(frame) = record(&mut outcome, &line) else {
            continue;
        };
        if let Some((request_id, tool)) = can_use_tool_request(&frame) {
            let reason = match decided_permission(spec.depth, &tool) {
                // marion already knows the answer, so there is nothing for a wait to produce.
                Some(refusal) => refusal,
                None => {
                    // Nobody to ask. Consume the episode's budget, then deny and let the node
                    // proceed.
                    std::thread::sleep(spec.blocked_bound);
                    NO_ANSWERER
                }
            };
            writeln!(stdin, "{}", deny_response(&request_id, reason))?;
            stdin.flush()?;
            outcome.denied_permissions.push(tool);
        } else if let Some((request_id, subtype)) = unanswerable_control_request(&frame) {
            // **Answered, not dropped.** See [`unsupported_request_response`]: a `control_request`
            // marion leaves unanswered hangs a root indefinitely, because a root has no wall clock.
            writeln!(
                stdin,
                "{}",
                unsupported_request_response(&request_id, &subtype)
            )?;
            stdin.flush()?;
            outcome.unanswered_control_requests.push(subtype);
        }
        let terminal = frame.get("type").and_then(Value::as_str) == Some("result");
        outcome.transcript.push(frame);
        if terminal {
            break;
        }
    }

    // Closing stdin is what ends a `--input-format stream-json` session.
    drop(stdin);
    let status = child.wait()?;
    outcome.stderr = stop(false, watchdog, &mut child, stderr);
    outcome.exit_code = status.code();
    outcome.signal = status.signal();
    outcome.timed_out = expired.load(Ordering::Relaxed);
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::agent_type::builtin;
    use marion_core::harness::Harness;
    use marion_harness::adapter_for;
    use marion_testsupport::scratch;

    /// **The routing rule, asserted the way §3.4 requires it to be written.** Not one arm of this
    /// matches on a harness name: the expectation is derived from the adapter's own
    /// `ExecutionSurfaces`, so a fifth harness gets the right path by declaring its surfaces and
    /// this test keeps passing without being edited.
    ///
    /// It covers **both** axes at once, which is the point of there being one derivation: the same
    /// answer routes `marion run <type>` and `run_spawn`.
    #[test]
    fn every_harness_routes_to_the_path_its_surfaces_select_and_not_to_one_named_for_it() {
        for h in Harness::ALL {
            let surfaces = adapter_for(h).unwrap().surfaces();
            let expected = match surfaces.control {
                ControlTransport::Typed(_) => Some(LaunchPath::Duplex),
                ControlTransport::LaunchOnly => Some(LaunchPath::LaunchOnly),
                ControlTransport::TerminalInput => None,
            };
            assert_eq!(launch_path(&surfaces), expected, "{h}");
            // And the derivation agrees with §3.4's plane table it has to agree with: a duplex node
            // is exactly a node with a typed control plane.
            assert_eq!(
                launch_path(&surfaces) == Some(LaunchPath::Duplex),
                surfaces.has_typed_control_plane(),
                "{h}: the launch path and the plane derivation must read the same axis"
            );
        }
    }

    /// The four built-ins, each landing where its harness's measured surface puts it — stated for
    /// **children**, which is the axis that was wrong: `run_spawn` drove all four as `LaunchOnly`,
    /// and claude-code is not one, so its turn went out toolless and it exited 0 having called
    /// nothing. Separate from the rule above because *which* built-in is on which path is a fact
    /// about today's harnesses, and a regression that flipped one would otherwise be invisible.
    #[test]
    fn the_builtin_agent_types_land_on_the_paths_their_harnesses_afford() {
        let got: Vec<(&str, Option<LaunchPath>)> =
            ["claude", "codex", "codex-impl", "gemini", "opencode"]
                .into_iter()
                .map(|n| {
                    let h = builtin(n).expect("built-in resolves").harness;
                    (n, launch_path(&adapter_for(h).unwrap().surfaces()))
                })
                .collect();
        assert_eq!(
            got,
            vec![
                ("claude", Some(LaunchPath::Duplex)),
                ("codex", Some(LaunchPath::LaunchOnly)),
                ("codex-impl", Some(LaunchPath::LaunchOnly)),
                ("gemini", Some(LaunchPath::LaunchOnly)),
                ("opencode", Some(LaunchPath::LaunchOnly)),
            ]
        );
    }

    /// The two arms of the frame loop are disjoint, and the unknown arm reads only the two fields
    /// §5.2 makes normative for the inbound direction.
    #[test]
    fn an_inbound_control_request_marion_cannot_serve_is_recognised_and_can_use_tool_is_not() {
        let dialog: Value = serde_json::from_str(
            r#"{"type":"control_request","request_id":"d-1","request":{"subtype":"request_user_dialog"}}"#,
        )
        .unwrap();
        assert_eq!(
            unanswerable_control_request(&dialog),
            Some(("d-1".to_string(), "request_user_dialog".to_string()))
        );
        // The kind marion *does* answer must never fall into the generic arm.
        let ask: Value = serde_json::from_str(
            r#"{"type":"control_request","request_id":"<UUID-4>","request":{"subtype":"can_use_tool","tool_name":"Bash"}}"#,
        )
        .unwrap();
        assert_eq!(unanswerable_control_request(&ask), None);
        // Nor may an *outbound* reply or a cancel be mistaken for a request to answer.
        let reply: Value = serde_json::from_str(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"a","response":{}}}"#,
        )
        .unwrap();
        assert_eq!(unanswerable_control_request(&reply), None);
        let cancel: Value =
            serde_json::from_str(r#"{"type":"control_cancel_request","request_id":"d-1"}"#)
                .unwrap();
        assert_eq!(unanswerable_control_request(&cancel), None);
        // A frame with no top-level `request_id` cannot be answered or cancelled (§5.2), so it is
        // left alone rather than answered against an invented id.
        let anonymous: Value =
            serde_json::from_str(r#"{"type":"control_request","request":{"subtype":"hook"}}"#)
                .unwrap();
        assert_eq!(unanswerable_control_request(&anonymous), None);
    }

    /// The envelope of the generic answer. `error`, not `success`: marion produced no answer, and a
    /// `success` must carry a payload whose shape is specific to a subtype marion has never seen.
    #[test]
    fn the_generic_answer_is_an_error_subtype_naming_the_kind_and_correlating_on_the_request_id() {
        let v: Value =
            serde_json::from_str(&unsupported_request_response("d-1", "request_user_dialog"))
                .unwrap();
        assert_eq!(v["type"], "control_response");
        assert_eq!(v["response"]["request_id"], "d-1");
        assert_eq!(v["response"]["subtype"], "error");
        assert!(v["response"]["response"].is_null(), "no invented payload");
        let msg = v["response"]["error"].as_str().unwrap();
        assert!(msg.contains("request_user_dialog"), "{msg}");
        assert!(msg.contains("§11 item 14"), "{msg}");
        // A subtype-less request still gets an answer, since the hang is what matters.
        let v: Value = serde_json::from_str(&unsupported_request_response("d-2", "")).unwrap();
        assert_eq!(v["response"]["request_id"], "d-2");
        assert_eq!(v["response"]["subtype"], "error");
    }

    /// **The hang.** A node emits a `control_request` of a kind marion does not implement and then
    /// blocks on stdin, exactly as §5.2 says the CLI does. The node here is launched the way a
    /// **root** is — `wall_clock: None`, §9 — so before this fix nothing bounded the wait at all:
    /// `blocked_bound` is a `sleep` inside the `can_use_tool` arm, not a watchdog.
    ///
    /// The stub carries its own 10 s self-destruct so a regression **fails** rather than wedging
    /// the suite, and the elapsed assertion is what distinguishes the two: answered is immediate,
    /// unanswered is the self-destruct.
    #[test]
    fn an_unknown_inbound_control_request_is_answered_and_a_root_with_no_wall_clock_does_not_hang()
    {
        let dir = scratch("duplex-unknown");
        let marker = dir.join("mcp-ready");
        std::fs::write(&marker, b"ready\n").unwrap();
        let answer = dir.join("answer.jsonl");
        // Answer `initialize`, take the prompt, then ask something marion has no implementation
        // for and **block on stdin for the reply**. The reply is written out for inspection; with
        // no reply the shell sits in `read` until its own watchdog kills it.
        let script = format!(
            r#"( sleep 10; kill -9 $$ ) 2>/dev/null &
read init
printf '{{"type":"control_response","response":{{"subtype":"success","request_id":"marion-init-test","response":{{}}}}}}\n'
read user
printf '{{"type":"control_request","request_id":"dialog-1","request":{{"subtype":"request_user_dialog"}}}}\n'
read reply
printf '%s\n' "$reply" > '{}'
printf '{{"type":"result","subtype":"success"}}\n'"#,
            answer.display()
        );
        let started = Instant::now();
        let out = run_duplex(
            SysCommand::new("sh").args(["-c", &script]),
            &DuplexSpec {
                ready_file: &marker,
                prompt: "do the task",
                init_id: "marion-init-test".into(),
                mcp_ready_timeout: StdDuration::from_secs(5),
                blocked_bound: StdDuration::from_secs(900),
                depth: crate::root::ROOT_DEPTH,
                // A **root**: §9 offers it no wall-clock ceiling, so nothing here can rescue a
                // dropped request. That is the whole point of the test.
                wall_clock: None,
                sink: None,
                on_started: None,
            },
        )
        .expect("the run returns");
        let elapsed = started.elapsed();
        let written = std::fs::read_to_string(&answer).unwrap_or_default();
        assert!(
            !written.trim().is_empty(),
            "marion dropped an inbound control_request instead of answering it: the node waited \
             for a control_response that never came ({elapsed:?})"
        );
        let reply: Value = serde_json::from_str(written.trim()).expect("the answer is a frame");
        assert_eq!(reply["type"], "control_response");
        assert_eq!(
            reply["response"]["request_id"], "dialog-1",
            "the answer must correlate to the request that was asked"
        );
        assert_eq!(reply["response"]["subtype"], "error");
        assert!(
            elapsed < StdDuration::from_secs(5),
            "the root did not proceed promptly: {elapsed:?}"
        );
        assert!(
            out.transcript
                .iter()
                .any(|f| f.get("type").and_then(Value::as_str) == Some("result")),
            "the node reached its terminal frame"
        );
        assert_eq!(out.unanswered_control_requests, vec!["request_user_dialog"]);
        // `blocked_bound` above is 900 s: the generic arm must not spend a permission budget it is
        // not a permission, which the elapsed assertion already proves.
        assert!(out.denied_permissions.is_empty());
    }

    /// A node that asks permission for one tool, hands back marion's answer, and finishes.
    ///
    /// The ask carries the three fields S9 measured as common to every `can_use_tool` 2.1.220
    /// emits, and nothing else: what marion reads is what a real one would have carried.
    fn permission_asking_node(marker: &Path, tool: &str, answer: &Path) -> String {
        std::fs::write(marker, b"ready\n").unwrap();
        format!(
            r#"( sleep 30; kill -9 $$ ) 2>/dev/null &
read init
printf '{{"type":"control_response","response":{{"subtype":"success","request_id":"marion-init-test","response":{{}}}}}}\n'
read user
printf '{{"type":"control_request","request_id":"ask-1","request":{{"subtype":"can_use_tool","tool_name":"{tool}"}}}}\n'
read reply
printf '%s\n' "$reply" > '{answer}'
printf '{{"type":"result","subtype":"success"}}\n'"#,
            answer = answer.display()
        )
    }

    /// The `response.message` of the denial marion wrote, or a panic naming what it wrote instead.
    fn denial_message(answer: &Path) -> String {
        let written = std::fs::read_to_string(answer).unwrap_or_default();
        let reply: Value = serde_json::from_str(written.trim())
            .unwrap_or_else(|e| panic!("marion never answered the permission ask ({e})"));
        assert_eq!(reply["response"]["request_id"], "ask-1");
        assert_eq!(
            reply["response"]["response"]["behavior"], "deny",
            "M1 denies; the question here is only how long it takes and what it says: {reply}"
        );
        reply["response"]["response"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// The bound this pair of tests spends, or refuses to. Long enough that a `sleep` of it is
    /// unmistakable next to an immediate answer, short enough not to slow the suite when it is
    /// deliberately spent.
    const PROBE_BOUND: StdDuration = StdDuration::from_secs(5);

    /// **An ask marion can decide is decided, not waited out.**
    ///
    /// §5.4: `report` is self-only and only on a node that has a contract — *"rejected on a root"*.
    /// A root's `report` is therefore not the kind of ask §9's block-then-deny rule is written for:
    /// that rule is for asks with **nobody to ask**, and this one has a known answer that depends on
    /// nothing but which node is calling.
    ///
    /// Before this, marion slept the whole `Blocked` bound first and then answered *"no permission
    /// answerer in M1; the node's Blocked bound expired"*. On a real root that bound defaults to
    /// **900 s** (`bin/marion.rs`'s `blocked_bound_secs`), so a decidable rule violation cost a
    /// fifteen-minute stall and then blamed a missing answerer for something marion could have
    /// decided instantly. Both halves are asserted, because either alone would pass against half
    /// the bug: the answer is immediate **and** it says why.
    #[test]
    fn a_roots_report_is_denied_at_once_in_5_4s_terms_rather_than_costing_the_blocked_bound() {
        let dir = scratch("duplex-decidable");
        let marker = dir.join("mcp-ready");
        let answer = dir.join("answer.jsonl");
        // The harness's own spelling of marion's verb, from the adapter that owns that mapping.
        let tool = adapter_for(Harness::ClaudeCode)
            .expect("claude has an adapter")
            .marion_tool_name("report");
        let started = Instant::now();
        let out = run_duplex(
            SysCommand::new("sh").args(["-c", &permission_asking_node(&marker, &tool, &answer)]),
            &DuplexSpec {
                ready_file: &marker,
                prompt: "do the task",
                init_id: "marion-init-test".into(),
                mcp_ready_timeout: StdDuration::from_secs(5),
                blocked_bound: PROBE_BOUND,
                depth: crate::root::ROOT_DEPTH,
                // A root: §9 offers it no wall clock, so nothing but the answer ends this run.
                wall_clock: None,
                sink: None,
                on_started: None,
            },
        )
        .expect("the run returns");
        let elapsed = started.elapsed();
        let message = denial_message(&answer);

        assert!(
            elapsed < PROBE_BOUND,
            "the `Blocked` bound is for an ask marion cannot answer; this one it can, and spending \
             the bound on it stalls a root for as long as the operator's budget says ({elapsed:?} \
             of {PROBE_BOUND:?})"
        );
        for needle in ["§5.4", "contract", "root"] {
            assert!(
                message.contains(needle),
                "the denial must give §5.4's reason ({needle:?} missing), not blame a missing \
                 answerer for a rule marion decided: {message}"
            );
        }
        assert!(
            !message.contains("expired"),
            "nothing expired — the answer was known before the ask arrived: {message}"
        );
        assert_eq!(out.denied_permissions, vec![tool]);
        assert!(
            out.transcript
                .iter()
                .any(|f| f.get("type").and_then(Value::as_str) == Some("result")),
            "§9: a denied node proceeds — it is not killed"
        );
    }

    /// **And an ask marion genuinely cannot answer still spends the bound**, unchanged.
    ///
    /// This is the half a careless fix breaks. §9's rule stands for every ask with nobody to ask:
    /// the node's `Blocked` budget is held, *then* the request is denied and the node proceeds. A
    /// built-in `Bash` call is such an ask — no rule in §5.4 decides it, and only an operator could
    /// — so it must arrive at the same denial it always has, at the same cost.
    #[test]
    fn an_ask_marion_cannot_answer_still_holds_the_blocked_bound_before_denying() {
        let dir = scratch("duplex-undecidable");
        let marker = dir.join("mcp-ready");
        let answer = dir.join("answer.jsonl");
        let started = Instant::now();
        let out = run_duplex(
            SysCommand::new("sh").args(["-c", &permission_asking_node(&marker, "Bash", &answer)]),
            &DuplexSpec {
                ready_file: &marker,
                prompt: "do the task",
                init_id: "marion-init-test".into(),
                mcp_ready_timeout: StdDuration::from_secs(5),
                blocked_bound: PROBE_BOUND,
                depth: crate::root::ROOT_DEPTH,
                wall_clock: None,
                sink: None,
                on_started: None,
            },
        )
        .expect("the run returns");
        let elapsed = started.elapsed();
        let message = denial_message(&answer);
        assert!(
            elapsed >= PROBE_BOUND,
            "§9: an ask with nobody to ask is held for the node's whole Blocked budget before it is \
             denied ({elapsed:?})"
        );
        assert!(
            message.contains("no permission answerer"),
            "and the reason is still the honest one for this kind of ask: {message}"
        );
        assert_eq!(out.denied_permissions, vec!["Bash".to_string()]);
    }

    /// Set on the re-executed copy of this test binary that actually drives a child. See
    /// [`a_child_run_writes_not_one_byte_of_the_nodes_stream_to_marions_own_stdout`].
    const CHILD_STDOUT_PROBE: &str = "MARION_DUPLEX_CHILD_STDOUT_PROBE";
    /// A string the stub node says on stdout and marion must never repeat on its own.
    const SENTINEL: &str = "sentinel-4f1a-the-node-said-this";

    /// A node whose stream contains the sentinel in a frame, in a non-JSON line, and in a tool call.
    fn sentinel_node(marker: &Path) -> String {
        std::fs::write(marker, b"ready\n").unwrap();
        format!(
            r#"read init
printf '{{"type":"control_response","response":{{"subtype":"success","request_id":"marion-init-probe","response":{{}}}}}}\n'
read user
printf '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{SENTINEL}"}}]}}}}\n'
printf 'this line is not json at all: {SENTINEL}\n'
printf '{{"type":"result","subtype":"success","result":"{SENTINEL}"}}\n'"#
        )
    }

    fn drive_sentinel_node(spec_sink: Option<StreamSink<'_>>, marker: &Path) -> DuplexOutcome {
        run_duplex(
            SysCommand::new("sh").args(["-c", &sentinel_node(marker)]),
            &DuplexSpec {
                ready_file: marker,
                prompt: "do the task",
                init_id: "marion-init-probe".into(),
                mcp_ready_timeout: StdDuration::from_secs(5),
                blocked_bound: StdDuration::ZERO,
                depth: crate::root::ROOT_DEPTH + 1,
                // A **child**: bounded, exactly as `run_spawn` bounds one.
                wall_clock: Some(StdDuration::from_secs(30)),
                sink: spec_sink,
                on_started: None,
            },
        )
        .expect("the run returns")
    }

    /// **The load-bearing test of the streaming change.** A child is driven from inside
    /// `marion-supervisor`, whose stdout *is* the stdio MCP stream the root harness parses — so a
    /// driver that prints a frame as it reads it does not merely produce noise, it injects
    /// non-JSON-RPC bytes into a protocol marion itself is speaking, and the root's tool calls stop
    /// working for a reason nothing reports.
    ///
    /// It cannot be asserted in-process: libtest captures `println!` from the test thread, so an
    /// in-process check would pass against a driver that prints unconditionally. So the driver runs
    /// in a **re-executed copy of this test binary** with `--nocapture`, where a stray write reaches
    /// a real pipe, and the parent asserts the sentinel the node said is nowhere in it.
    ///
    /// Mutation check (2026-08-05): with `sink` ignored and the frame `println!`ed unconditionally
    /// in `record`, this fails —
    /// *"marion echoed a child node's stream onto its own stdout"* — and passes as written.
    #[test]
    fn a_child_run_writes_not_one_byte_of_the_nodes_stream_to_marions_own_stdout() {
        if std::env::var(CHILD_STDOUT_PROBE).is_ok() {
            // The re-executed copy: drive a child with no sink and say nothing at all. Whatever
            // reaches this process's stdout from here on is marion's doing, not the test's.
            let dir = scratch("duplex-child-silence-probe");
            let out = drive_sentinel_node(None, &dir.join("mcp-ready"));
            // The stub must really have spoken, or the parent's assertion would hold vacuously.
            assert!(
                out.stdout.contains(SENTINEL),
                "the stub node never emitted the sentinel; the silence assertion would be vacuous"
            );
            return;
        }
        let exe = std::env::current_exe().expect("the test binary re-executes itself");
        // libtest names a test by its path without the crate segment.
        let name = format!(
            "{}::a_child_run_writes_not_one_byte_of_the_nodes_stream_to_marions_own_stdout",
            module_path!().split_once("::").expect("crate::module").1
        );
        let probe = SysCommand::new(&exe)
            .args(["--exact", "--nocapture", "--test-threads", "1", &name])
            .env(CHILD_STDOUT_PROBE, "1")
            .output()
            .expect("the probe runs");
        let stdout = String::from_utf8_lossy(&probe.stdout);
        let stderr = String::from_utf8_lossy(&probe.stderr);
        assert!(
            probe.status.success(),
            "the probe itself failed, so it proves nothing:\n{stdout}\n{stderr}"
        );
        assert!(
            !stdout.contains(SENTINEL),
            "marion echoed a child node's stream onto its own stdout — the stdio MCP stream the \
             root harness parses. Every byte there must be a JSON-RPC message marion wrote on \
             purpose.\n{stdout}"
        );
        assert!(
            !stderr.contains(SENTINEL),
            "marion echoed a child node's stream onto its own stderr; a child's output belongs in \
             its captured record, not in the supervisor's.\n{stderr}"
        );
    }

    /// The other half of the seam: with a sink, **every** line the node wrote is delivered live —
    /// frames as frames, and a line that was not JSON as itself rather than dropped. Delivery is in
    /// arrival order and happens before the run returns, which is the whole point.
    #[test]
    fn a_sink_sees_every_line_in_arrival_order_including_one_that_is_not_json() {
        let dir = scratch("duplex-sink-seam");
        let seen: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let sink = |e: StreamEvent<'_>| {
            seen.borrow_mut().push(match e {
                Frame(v) => format!("frame:{}", v["type"].as_str().unwrap_or("?")),
                Unparsed(s) => format!("unparsed:{s}"),
            });
        };
        use StreamEvent::{Frame, Unparsed};
        let out = drive_sentinel_node(Some(&sink), &dir.join("mcp-ready"));
        let seen = seen.into_inner();
        assert_eq!(
            seen.iter().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "frame:control_response",
                "frame:assistant",
                &format!("unparsed:this line is not json at all: {SENTINEL}"),
                "frame:result",
            ],
            "the sink must see the initialize round trip and the turn, in arrival order"
        );
        // Streaming is additive: the accumulated record is untouched by the presence of a sink.
        assert_eq!(out.transcript.len(), 3);
        assert!(out.stdout.contains("not json at all"));
    }

    /// A pty-only surface has no launch path on either axis, and §5.2 forbids inventing one by
    /// handing a headless node a pty on stdin. `TerminalInput` is unreachable from today's four
    /// adapters, so this is asserted at the derivation rather than through one.
    #[test]
    fn a_terminal_input_surface_is_refused_rather_than_pushed_down_one_of_the_two_paths() {
        assert_eq!(launch_path(&ExecutionSurfaces::opaque()), None);
        assert_eq!(launch_path(&ExecutionSurfaces::interactive()), None);
    }

    #[test]
    fn a_user_turn_matches_the_shape_s1_recorded() {
        let v: Value = serde_json::from_str(&user_message("delegate")).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"][0]["text"], "delegate");
        assert!(v["parent_tool_use_id"].is_null());
    }

    #[test]
    fn the_initialize_round_trip_is_matched_on_the_request_id_not_the_subtype() {
        let frame: Value = serde_json::from_str(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"a","response":{}}}"#,
        )
        .unwrap();
        assert!(is_control_response_to(&frame, "a"));
        assert!(
            !is_control_response_to(&frame, "b"),
            "another node's reply must not satisfy our wait"
        );
        let init: Value = serde_json::from_str(&initialize_request("a")).unwrap();
        assert_eq!(init["request_id"], "a");
        assert_eq!(init["request"]["subtype"], "initialize");
    }

    #[test]
    fn a_permission_ask_is_recognised_by_its_subtype_and_carries_its_request_id() {
        // Verbatim from tests/fixtures/s9/can-use-tool-deny.stdout.jsonl, recorded off a real
        // 2.1.220. The top-level request_id is load-bearing: a frame without one could be neither
        // answered nor cancelled.
        let frame: Value = serde_json::from_str(
            r#"{"type":"control_request","request_id":"<UUID-4>","request":{"subtype":"can_use_tool","tool_name":"mcp__marion__report","display_name":"Report","input":{"narrative":"s9 probe: a verb the root may not use"},"permission_suggestions":[{"type":"addRules","rules":[{"toolName":"mcp__marion__report"}],"behavior":"allow","destination":"localSettings"}],"tool_use_id":"toolu_marion_spawn_1"}}"#,
        )
        .unwrap();
        assert_eq!(
            can_use_tool_request(&frame),
            Some(("<UUID-4>".to_string(), "mcp__marion__report".to_string()))
        );
        let other: Value = serde_json::from_str(
            r#"{"type":"control_request","request_id":"r","request":{"subtype":"interrupt"}}"#,
        )
        .unwrap();
        assert!(can_use_tool_request(&other).is_none());
    }

    #[test]
    fn an_unanswerable_permission_is_denied_and_the_node_is_not_killed() {
        let v: Value = serde_json::from_str(&deny_response("req_7", "bound expired")).unwrap();
        assert_eq!(v["response"]["request_id"], "req_7");
        assert_eq!(
            v["response"]["response"]["behavior"], "deny",
            "§9: on expiry marion denies the pending permission and lets the node proceed"
        );
        // The envelope the CLI actually accepted (S9): the response's own `subtype` is `success` —
        // it reports that the *answer* was produced, not that the permission was granted. Sending
        // `subtype: "deny"` here would be a protocol error, not a denial.
        assert_eq!(v["response"]["subtype"], "success");
    }

    #[test]
    fn a_ready_marker_that_never_appears_is_a_refusal_not_a_silent_first_turn() {
        let missing = std::env::temp_dir().join(format!("marion-never-{}", std::process::id()));
        let _ = std::fs::remove_file(&missing);
        assert!(!wait_for_ready(&missing, StdDuration::from_millis(30)));
        let e = DuplexError::McpNeverReady(StdDuration::from_millis(30), missing);
        assert!(e.to_string().contains("without marion's tools"));
        assert!(e.to_string().contains("refused"));
    }

    /// The marker lives **inside a [`temp`] dir**, exactly as the gate test below already puts its
    /// own there, rather than beside one in the system temp root with a `remove_file` at the end.
    ///
    /// That trailing call cleaned up only the passing run: a failing assertion unwinds straight
    /// past it, so the marker survived on precisely the runs a developer re-runs — the same
    /// asymmetry cb6ab2e removed from `temp` itself, one file at a time instead of one dir. The
    /// guard already exists and already covers a file placed under it, so this needs no second
    /// guard type; it needs the marker to be somewhere something owns.
    ///
    /// `dir` is bound for the whole test on purpose. `scratch("duplex-ready").join(…)` would drop the guard
    /// at the end of that statement and delete the directory out from under `wait_for_ready`.
    #[test]
    fn a_marker_written_after_the_wait_begins_is_still_seen() {
        let dir = scratch("duplex-ready");
        let path = dir.join("mcp-ready");
        let p = path.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(StdDuration::from_millis(50));
            std::fs::write(&p, b"").unwrap();
        });
        assert!(wait_for_ready(&path, StdDuration::from_secs(5)));
        // Joined before the guard drops, so the writing thread cannot still be holding a path
        // inside a directory this test has already removed.
        h.join().unwrap();
    }

    /// The gate refuses **before** anything is written to the node's stdin: a marker that never
    /// lands must not become a prompt that goes out anyway.
    #[test]
    fn a_node_whose_marker_never_lands_is_killed_and_the_run_refused() {
        let dir = scratch("duplex-gate");
        let marker = dir.join("mcp-ready");
        let seen = dir.join("stdin-seen");
        // A node that echoes anything it is given. It is never given anything, because the marker
        // never appears — that is the assertion.
        let err = run_duplex(
            SysCommand::new("sh").args(["-c", &format!("cat > '{}'; sleep 30", seen.display())]),
            &DuplexSpec {
                ready_file: &marker,
                prompt: "do the task",
                init_id: "marion-init-test".into(),
                mcp_ready_timeout: StdDuration::from_millis(100),
                blocked_bound: StdDuration::ZERO,
                depth: crate::root::ROOT_DEPTH + 1,
                wall_clock: Some(StdDuration::from_secs(10)),
                sink: None,
                on_started: None,
            },
        )
        .expect_err("a node that never got marion's tools must be refused");
        assert!(matches!(err, DuplexError::McpNeverReady(_, _)), "{err}");
        assert!(
            !seen.exists()
                || std::fs::read_to_string(&seen)
                    .unwrap_or_default()
                    .is_empty(),
            "the prompt was written even though the gate never opened"
        );
    }

    /// The wall clock, and the group kill behind it. A child that hangs between frames must still
    /// be killed — which is why the bound is a watchdog and not a per-read deadline.
    #[test]
    fn a_child_that_hangs_between_frames_is_killed_on_its_wall_clock() {
        let dir = scratch("duplex-hang");
        let marker = dir.join("mcp-ready");
        std::fs::write(&marker, b"ready\n").unwrap();
        // Answers the initialize round trip, then hangs forever without ever emitting `result`.
        let script = r#"read line; printf '{"type":"control_response","response":{"subtype":"success","request_id":"marion-init-test","response":{}}}\n'; sleep 600"#;
        let started = Instant::now();
        let out = run_duplex(
            SysCommand::new("sh").args(["-c", script]),
            &DuplexSpec {
                ready_file: &marker,
                prompt: "do the task",
                init_id: "marion-init-test".into(),
                mcp_ready_timeout: StdDuration::from_secs(5),
                blocked_bound: StdDuration::ZERO,
                depth: crate::root::ROOT_DEPTH + 1,
                wall_clock: Some(StdDuration::from_millis(500)),
                sink: None,
                on_started: None,
            },
        )
        .expect("the bounded run returns");
        assert!(
            started.elapsed() < StdDuration::from_secs(30),
            "the wall clock did not fire: {:?}",
            started.elapsed()
        );
        assert!(out.timed_out, "marion's own attributed kill (§6.7)");
        assert_eq!(out.signal, Some(9));
    }
}
