//! `marion run` shows the root's work **while it is happening**, end to end through the real
//! `marion` binary against a stub duplex harness.
//!
//! Until this existed a run was a black box for its whole duration: `bin/marion.rs` printed the
//! accumulated transcript after `root::launch` returned, so a person watching a root delegate to a
//! child saw a blank terminal for minutes and could not tell work from a wedge.
//!
//! Three properties, and each is the one that fails if the change is quietly undone:
//!
//! 1. **Live.** A rendered line for the root's `spawn` reaches the terminal *while the node is
//!    still running* — asserted by reading marion's stderr incrementally and checking the process
//!    has not exited yet, not by inspecting the output after the fact, which a
//!    print-it-all-at-the-end implementation would also pass.
//! 2. **Separated.** The human stream is on **stderr**; **stdout stays the machine surface it
//!    was** — every line on it still parses as a frame, because a root has no `TaskContract` to
//!    return (§9), its stream *is* its result, and `launch_only_root.rs` reads that stdout back
//!    with `marion_tool_calls`.
//! 3. **Additive.** The accumulated transcript on stdout is unchanged by the presence of the live
//!    view: the same frames, in the same order.
//!
//! # Why a stub harness
//!
//! What is under test is marion's rendering of a stream, so the *stream* has to be the input —
//! including a node that pauses mid-run (property 1 needs a live process to catch) and one that
//! writes a line that is not JSON (a kind no real CLI emits on demand). The stub speaks the real
//! §6.1 step 8 protocol — it finds its ready marker in the `--mcp-config` document marion compiled,
//! answers the `initialize` round trip against **marion's own** `request_id`, and waits for the
//! user frame — so the launch path exercised here is the real one, not a shortcut around it.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use marion_testsupport::scratch;
use serde_json::Value;

/// Generous. It exists so a hung `marion` fails loudly instead of wedging the suite.
const RUN_BOUND: Duration = Duration::from_secs(60);

/// How long the stub pauses after announcing its `spawn`, which is the window property 1 catches
/// the run alive in. Long enough to survive a loaded machine, short enough to pay once.
const PAUSE_SECS: u64 = 3;

/// The text the stub's root "says", distinctive enough to find in either stream.
const SAID: &str = "Delegating to a codex child.";

/// A stub `claude` on a directory prepended to `PATH`, speaking the duplex protocol.
///
/// The ready marker is **not** passed to it: the real CLI starts marion's bridge, and the bridge
/// touches the marker. The stub finds the path where the CLI would — in the `--mcp-config` document
/// marion compiled — which is what makes this a test of the launch path and not of a rearrangement
/// of it.
fn stub_claude(dir: &Path) -> std::path::PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let program = bin.join("claude");
    std::fs::write(
        &program,
        format!(
            r#"#!/bin/sh
cfg=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--mcp-config" ]; then cfg="$a"; fi
  prev="$a"
done
ready=$(sed -n 's/.*"MARION_READY_FILE"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$cfg" | head -1)
if [ -z "$ready" ]; then echo "stub: no MARION_READY_FILE in $cfg" >&2; exit 3; fi
: > "$ready"

read init
rid=$(printf '%s' "$init" | sed -n 's/.*"request_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
printf '{{"type":"control_response","response":{{"subtype":"success","request_id":"%s","response":{{"commands":[]}}}}}}\n' "$rid"

read user
printf '{{"type":"system","subtype":"init","model":"stub-model","tools":["Task"],"mcp_servers":[{{"name":"marion","status":"connected"}}]}}\n'
printf '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{SAID}"}}]}}}}\n'
printf '{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"toolu_1","name":"mcp__marion__spawn","input":{{"agent_type":"codex","prompt":"fix the failing test"}}}}]}}}}\n'
sleep {PAUSE_SECS}
printf '{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_1","content":"the child reported: done"}}]}}}}\n'
printf '{{"type":"a_kind_from_a_future_cli","subtype":"nobody_has_measured_this"}}\n'
printf 'this line is not json at all\n'
printf '{{"type":"result","subtype":"success","is_error":false,"duration_ms":4200,"num_turns":2}}\n'
"#
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// Kill `child` if it outlives the bound, so a regression that hangs fails this test rather than
/// the suite.
fn reap_after(child: &mut Child, bound: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + bound;
    loop {
        match child.try_wait().expect("the child is waitable") {
            Some(status) => return Some(status),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[test]
fn a_root_is_rendered_to_stderr_while_it_runs_and_stdout_stays_a_frame_stream() {
    let dir = scratch("run-stream");
    let repo = dir.join("repo");
    let state = dir.join("state");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let path = format!(
        "{}:{}",
        stub_claude(&dir).display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args([
            "run",
            "claude",
            "--prompt",
            "Delegate the task to a child.",
            "--repo",
            &repo.to_string_lossy(),
            "--state-dir",
            &state.to_string_lossy(),
            "--canned",
            "--base-url",
            // Nothing dials it: the stub makes no model call. It is here because --base-url is
            // what keeps a real credential out of this run.
            "http://127.0.0.1:9/v1",
            "--timeout",
            "30",
        ])
        .env("PATH", &path)
        .current_dir(&repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("marion run starts");

    // ---- property 1: the spawn is on the terminal before the run is over. ----------------------
    //
    // Read stderr line by line and stop at the rendered `spawn`. The stub is asleep for
    // PAUSE_SECS at that point, so a marion that printed only after `root::launch` returned would
    // reach this loop's end without ever having emitted the line — and, crucially, an
    // implementation that buffered the whole run would fail the liveness check below even though
    // the finished output looked identical.
    // The reading happens on a thread and the lines arrive over a channel, so the *waiting* is
    // bounded: `read_line` on a live pipe blocks, and a marion that wedged would otherwise hang
    // this test forever. A duplex root has no wall clock of its own (§9), so there is nothing else
    // in the system that would end it.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut err = BufReader::new(child.stderr.take().expect("stderr is piped"));
    let reader = std::thread::spawn(move || {
        loop {
            let mut line = String::new();
            match err.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    if tx.send(line).is_err() {
                        return;
                    }
                }
            }
        }
    });

    let mut stderr_so_far = String::new();
    let mut alive_at_the_spawn = None;
    let deadline = Instant::now() + RUN_BOUND;
    while let Ok(line) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        stderr_so_far.push_str(&line);
        if line.contains("MARION") && line.contains("spawn") {
            alive_at_the_spawn = Some(child.try_wait().expect("waitable").is_none());
            break;
        }
    }
    if alive_at_the_spawn.is_none() {
        // Nothing more will arrive, and a wedged marion must not become a wedged suite.
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "marion never rendered the root's spawn on stderr — either a run is still a black box, \
             or the view is going somewhere a person is not looking (check stdout)\nstderr:\n\
             {stderr_so_far}"
        );
    }
    assert_eq!(
        alive_at_the_spawn,
        Some(true),
        "the spawn line arrived only after the run had already finished, which is the black box \
         this change exists to remove\nstderr:\n{stderr_so_far}"
    );

    // Drain the rest, then reap.
    let mut rest = String::new();
    while let Ok(line) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        rest.push_str(&line);
    }
    reader.join().expect("the stderr reader finishes");
    let stderr = format!("{stderr_so_far}{rest}");
    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read;
        let _ = out.read_to_string(&mut stdout);
    }
    let status = reap_after(&mut child, RUN_BOUND);
    let status = status.unwrap_or_else(|| {
        panic!("marion run did not finish inside {RUN_BOUND:?}\nstderr:\n{stderr}")
    });
    assert!(
        status.success(),
        "marion run exited {status:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // ---- property 2: stdout is still nothing but frames. ---------------------------------------
    let frames: Vec<Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| {
                panic!(
                    "a line on marion's stdout is not a frame ({e}). stdout is the machine surface \
                     a root's result arrives on and `launch_only_root.rs` parses it back:\n{l}"
                )
            })
        })
        .collect();
    // Anchored at the start of a line: the root's prose appears on stdout *inside* a frame, which
    // is correct and is property 3. What must not appear is a **rendered** line.
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            line.starts_with('{'),
            "a rendered line reached stdout, which is the machine surface:\n{line}"
        );
    }
    assert!(
        stderr.contains(SAID) && !stderr.lines().any(|l| l.starts_with('{')),
        "the root's prose belongs on stderr, rendered rather than as a frame:\n{stderr}"
    );

    // ---- property 3: the accumulated transcript is exactly what it always was. ------------------
    let kinds: Vec<&str> = frames
        .iter()
        .map(|f| f["type"].as_str().unwrap_or("(none)"))
        .collect();
    assert_eq!(
        kinds,
        vec![
            "control_response",
            "system",
            "assistant",
            "assistant",
            "user",
            "a_kind_from_a_future_cli",
            "result",
        ],
        "streaming is additive: the transcript on stdout must be the same frames in the same \
         order, including the kind marion does not understand\nstdout:\n{stdout}"
    );

    // ---- and the view itself, on stderr: readable, and never a JSON dump. -----------------------
    assert!(
        stderr.contains(SAID),
        "the root's prose is shown:\n{stderr}"
    );
    assert!(
        stderr.contains(r#"agent_type="codex""#),
        "the spawn's arguments are shown in a readable form:\n{stderr}"
    );
    assert!(
        stderr.contains("the child reported: done"),
        "the tool result is shown:\n{stderr}"
    );
    assert!(
        stderr.contains("a_kind_from_a_future_cli"),
        "a frame kind marion has no handling for must still produce a line — silence is \
         indistinguishable from nothing having happened:\n{stderr}"
    );
    assert!(
        stderr.contains("this line is not json at all"),
        "a stdout line that was not JSON is the node's words and must not vanish:\n{stderr}"
    );
    assert!(
        stderr.contains("2 turns"),
        "the run's verdict is the last thing a watcher sees:\n{stderr}"
    );
    for line in stderr.lines() {
        assert!(
            line.chars().count() <= 240,
            "a rendered line is long enough to be a JSON paste:\n{line}"
        );
    }
}
