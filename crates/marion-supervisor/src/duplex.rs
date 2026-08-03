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

/// What one duplex session needs, over and above the compiled [`marion_harness::Invocation`] the
/// caller has already turned into a `Command`.
#[derive(Debug, Clone)]
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
    /// A wall-clock ceiling, after which the node's whole process group is killed. `Some` for a
    /// child, which is bounded by its contract's `timeout_secs`; `None` for a root, which marion
    /// offers no wall clock at all (§9).
    pub wall_clock: Option<StdDuration>,
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
/// 5. frames until the terminal `result`, answering `can_use_tool` on the way;
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
    let record = |outcome: &mut DuplexOutcome, line: &str| -> Option<Value> {
        outcome.stdout.push_str(line);
        outcome.stdout.push('\n');
        serde_json::from_str::<Value>(line).ok()
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
            // Nobody to ask. Consume the episode's budget, then deny and let the node proceed.
            std::thread::sleep(spec.blocked_bound);
            writeln!(
                stdin,
                "{}",
                deny_response(
                    &request_id,
                    "marion: no permission answerer in M1; the node's Blocked bound expired",
                )
            )?;
            stdin.flush()?;
            outcome.denied_permissions.push(tool);
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

    #[test]
    fn a_marker_written_after_the_wait_begins_is_still_seen() {
        let path = std::env::temp_dir().join(format!("marion-ready-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let p = path.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(StdDuration::from_millis(50));
            std::fs::write(&p, b"").unwrap();
        });
        assert!(wait_for_ready(&path, StdDuration::from_secs(5)));
        h.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    /// The gate refuses **before** anything is written to the node's stdin: a marker that never
    /// lands must not become a prompt that goes out anyway.
    #[test]
    fn a_node_whose_marker_never_lands_is_killed_and_the_run_refused() {
        let dir = std::env::temp_dir().join(format!("marion-duplex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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
                wall_clock: Some(StdDuration::from_secs(10)),
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
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The wall clock, and the group kill behind it. A child that hangs between frames must still
    /// be killed — which is why the bound is a watchdog and not a per-read deadline.
    #[test]
    fn a_child_that_hangs_between_frames_is_killed_on_its_wall_clock() {
        let dir = std::env::temp_dir().join(format!("marion-duplex-hang-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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
                wall_clock: Some(StdDuration::from_millis(500)),
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
