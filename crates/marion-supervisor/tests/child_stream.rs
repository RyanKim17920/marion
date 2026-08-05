//! **A child's life, visible while it is being lived** — the journal's first production reader,
//! end to end through the real `marion` binary with a real `claude` root and a real `codex` child.
//!
//! `run_stream.rs` proved the *root's* frames reach a person as they arrive. It could prove nothing
//! about a child: a child is driven inside the bridge's own process (§10), whose stdout is an MCP
//! stream and which must stay silent, so nothing about it ever reaches `marion run` directly. The
//! whole of a child's life was one long gap between `MARION spawn …` and the contract coming back.
//!
//! Every one of those transitions was already on disk — the bridge writes `SpawnIntent`, `Spawned`,
//! `Exited`, `ContractPersisted` into the project's `journal.jsonl` — and nothing at runtime read
//! any of it. This is the test that says a person now sees them.
//!
//! **The property is ordering, not elapsed time.** The assertion is that the child's start reaches
//! the terminal *before the `spawn` call returns* — before the line that used to be the first news
//! of the child's existence. A view that rendered after the run, or that polled only when the
//! root's own frames arrived, fails that regardless of how fast the machine is; a wall-clock
//! assertion would instead measure the machine.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test child_stream
//! ```
//!
//! It needs real `claude` and `codex` on `PATH` and does **not** skip when they are missing, for
//! the reason `journal_wiring.rs` gives. Every model call is served by the CannedProvider: **no
//! paid tokens.**

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use marion_core::harness::Harness;
use marion_harness::adapter_for;
use marion_provider::{CannedServer, Config, RootScript, RootTurn, Script};
use marion_testsupport::{fixture_repo, scratch};
use serde_json::json;

/// Generous. It exists so a hung run fails loudly instead of wedging the suite.
const RUN_BOUND: Duration = Duration::from_secs(300);

/// The root's `--timeout`. On a duplex surface this is §9's per-episode `Blocked`-only budget and
/// **not** a wall clock, so it is short: an unanswerable permission must fail in seconds.
const ROOT_BLOCKED_SECS: &str = "5";

/// The child's own wall clock, through `spawn`'s `timeout_secs`.
const CHILD_TIMEOUT_SECS: u64 = 60;

/// Keys the root's half of the canned script, and appears in no child request.
const ROOT_MARKER: &str = "MARION-CHILD-STREAM-ROOT-TURN-4b17";

const NARRATIVE: &str = "Wrote the marker under src/ and reported back.";
const CHILD_FILE: &str = "src/child-stream-marker.txt";

/// The canned script for a `claude` root that delegates to a `codex` child.
fn script() -> Script {
    let claude = adapter_for(Harness::ClaudeCode).expect("claude has an adapter");
    Script {
        root: Some(RootScript {
            marker: ROOT_MARKER.into(),
            turn: RootTurn {
                tool: claude.marion_tool_name("spawn"),
                args: json!({
                    "agent_type": "codex-impl",
                    "prompt": "Add the marker file under src/ and report back.",
                    "acceptance_criteria": ["a file exists under src/ containing the marker"],
                    "writable_scope": ["src/**"],
                    "timeout_secs": CHILD_TIMEOUT_SECS,
                }),
                final_text: "The child completed the task and reported back.".into(),
            },
        }),
        // The child is codex: it applies a patch, then reports through marion's own verb.
        child_narrative: NARRATIVE.into(),
        child_patch: format!(
            "*** Begin Patch\n*** Add File: {CHILD_FILE}\n+marion child-stream marker\n*** End Patch"
        ),
        child_final_text: json!({"narrative": NARRATIVE, "result_commits": []}).to_string(),
        ..Script::default()
    }
}

/// The index of the first line satisfying `pred`, or `None`.
fn first(lines: &[String], pred: impl Fn(&str) -> bool) -> Option<usize> {
    lines.iter().position(|l| pred(l))
}

#[test]
fn a_childs_start_reaches_the_terminal_before_the_spawn_that_created_it_returns() {
    let dir = scratch("child-stream");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: dir.join("provider-requests.jsonl"),
        script: script(),
    })
    .expect("the canned provider binds");

    let mut child = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args([
            "run",
            "claude",
            "--prompt",
            &format!("{ROOT_MARKER}: delegate the marker-file task to a child."),
            "--repo",
            &repo.to_string_lossy(),
            "--state-dir",
            &state.to_string_lossy(),
            "--base-url",
            &server.base_url(),
            "--canned",
            "--timeout",
            ROOT_BLOCKED_SECS,
        ])
        .current_dir(&*dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("marion run starts");

    // Read stderr on its own thread so the *waiting* is bounded: a blocking `read_line` against a
    // wedged marion would otherwise hang the suite, and a duplex root has no wall clock (§9).
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

    // The one liveness fact worth recording *while* it is true: was the run still going when the
    // child's start appeared? Read after the fact, it is unknowable.
    let mut alive_at_the_start_line = None;
    let mut lines: Vec<String> = Vec::new();
    let deadline = Instant::now() + RUN_BOUND;
    while let Ok(line) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        let is_start = line.starts_with("CHILD") && line.contains("started");
        lines.push(line.trim_end().to_string());
        if is_start && alive_at_the_start_line.is_none() {
            alive_at_the_start_line = Some(child.try_wait().expect("waitable").is_none());
        }
    }
    let _ = reader.join();
    let status = child.wait().expect("marion run is reaped");
    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read;
        let _ = out.read_to_string(&mut stdout);
    }
    drop(server);
    let shown = lines.join("\n");

    assert!(
        status.success(),
        "marion run exited {status:?}\nstderr:\n{shown}\nstdout:\n{stdout}"
    );

    // ---- the property: the child's start is news, and it is news *early*. ----------------------
    let started = first(&lines, |l| l.starts_with("CHILD") && l.contains("started"))
        .unwrap_or_else(|| {
            panic!(
                "no child start reached the terminal. The bridge wrote SpawnIntent and Spawned to \
                 the journal for this run; nothing read them back, which is the gap this exists to \
                 close.\nstderr:\n{shown}"
            )
        });
    assert_eq!(
        alive_at_the_start_line,
        Some(true),
        "the child's start was rendered only after the run had already finished\nstderr:\n{shown}"
    );

    // The line that used to be the first news of the child: the contract, coming back through the
    // root's own frame stream. The start must precede it — that is the silence being closed.
    let returned = first(&lines, |l| l.contains("child returned")).unwrap_or_else(|| {
        panic!("the root never saw its spawn return a contract\nstderr:\n{shown}")
    });
    assert!(
        started < returned,
        "the child's start was shown at line {started} and its contract came back at line \
         {returned}: the start must be visible *before* the spawn returns, or the minutes in \
         between are still blank\nstderr:\n{shown}"
    );

    // ---- and its end, read out of the journal rather than out of the root's stream. -------------
    let exited = first(&lines, |l| l.contains("exited Ok")).unwrap_or_else(|| {
        panic!("the child's terminal transition never reached the terminal\nstderr:\n{shown}")
    });
    assert!(
        started < exited,
        "a child cannot exit before it starts:\n{shown}"
    );
    assert!(
        lines[started].contains("codex"),
        "the start names the child's own agent type, which is the only thing that distinguishes \
         one child from another: {:?}",
        lines[started]
    );
    // The root is not narrated back at the person watching it: its banner already did that.
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.starts_with("CHILD") && l.contains("started"))
            .count(),
        1,
        "exactly one child started in this run, and it must be announced exactly once\n{shown}"
    );
    // Nothing the viewer does may reach the machine surface.
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            line.starts_with('{'),
            "a rendered line reached stdout, which is the machine surface:\n{line}"
        );
    }
}
