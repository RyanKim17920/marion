//! `marion run` on a **`LaunchOnly` root** (design §3.4, §6.1 step 8, §9), end to end through the
//! real `marion` binary.
//!
//! Three properties, one per test:
//!
//! 1. a root whose turn never reached marion's bridge **fails loudly, naming the cause** — it does
//!    not exit 0 having emitted plain text, which §6.1 calls out as the failure the readiness gate
//!    exists for and §12 records happening for real;
//! 2. the same run with one `mcp_tool_call` in its stream **succeeds** — so property 1 is a check
//!    that can pass, not an assertion that always fires;
//! 3. the wall-clock bound is enforced and the process group is killed. **Not optional**: measured
//!    in S13, opencode never exits on a provider hang (a 500 still retrying at 90 s, a
//!    connection-refused still hung at 180 s, no backoff ceiling), so without a bound
//!    `marion run opencode` is an unbounded hang.
//!
//! # Why the harness is a stub and not the real `codex`
//!
//! What is under test is **marion's** launch path: does it put the prompt in argv, read the
//! stream, assert readiness post hoc, and bound the run? Every one of those is a property of
//! marion given a stream, and a stub harness lets the *stream* be the input — including the two
//! streams a real `codex` will not produce on demand (one that reaches nothing, and one that never
//! exits). `m1_hop` is where the real binaries run.
//!
//! The stub is named `codex` and reached through `PATH`, so the argv marion compiled is exercised
//! verbatim rather than restated here: the stub records what it was given, and the test reads it
//! back.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use marion_supervisor::run::run_bounded;

/// Generous. The bound exists so a hung `marion` fails loudly instead of wedging the suite.
const RUN_BOUND: Duration = Duration::from_secs(60);

fn scratch(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("marion-lo-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    p.canonicalize().expect("scratch canonicalises")
}

/// A stub `codex` on a directory that is prepended to `PATH`.
///
/// `body` is shell, run after the stub has recorded its own argv and env to `argv.txt`. Returning
/// the directory rather than the file keeps the caller from having to know the program's name — it
/// is the *adapter's* choice, not the test's.
fn stub_harness(dir: &Path, body: &str) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let program = bin.join("codex");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\n\
             {{ echo \"CODEX_HOME=$CODEX_HOME\"; echo \"KEY=$MARION_DUMMY_KEY\"; \
             for a in \"$@\"; do echo \"ARG=$a\"; done; }} > '{argv}'\n\
             {body}\n",
            argv = dir.join("argv.txt").display()
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

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    elapsed: Duration,
}

/// `marion run codex --prompt …` with the stub ahead of the real one on `PATH`.
fn marion_run(dir: &Path, bin: &Path, prompt: &str, timeout_secs: &str) -> Run {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let started = Instant::now();
    let out = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_marion"))
            .args([
                "run",
                "codex",
                "--prompt",
                prompt,
                "--repo",
                &repo.to_string_lossy(),
                "--state-dir",
                &state.to_string_lossy(),
                // Nothing listens here. The stub is the whole model side of this run, and a URL
                // that resolves would only invite a real request. `--canned` is what makes that
                // legible to the binary: a loopback endpoint under real vendor auth is refused at
                // argument parsing, because it aims the operator's credential at a fake server.
                "--canned",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "--timeout",
                timeout_secs,
            ])
            .env("PATH", path)
            .current_dir(dir),
        RUN_BOUND,
    )
    .expect("marion run starts");
    assert!(
        !out.timed_out,
        "marion run did not finish inside {RUN_BOUND:?} — it is the thing under test that is \
         supposed to be bounded\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Run {
        code: out.code,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        elapsed: started.elapsed(),
    }
}

/// One `mcp_tool_call` item in the shape `tests/fixtures/s6/` recorded, shell-quoted for the stub.
const REACHED_THE_BRIDGE: &str = r#"{"type":"item.completed","item":{"id":"item_0","type":"mcp_tool_call","server":"marion","tool":"spawn","arguments":{},"status":"completed"}}"#;

/// **The property this whole path exists for.** A `LaunchOnly` root that never called a marion
/// tool took its turn without marion's tools; §6.1 forbids letting that end as plain text, because
/// a toolless turn exits 0 with no diagnostic anywhere and reads exactly like success.
#[test]
fn a_launch_only_root_that_never_reached_the_bridge_fails_loudly_instead_of_exiting_zero() {
    let dir = scratch("silent");
    // The measured failure shape, reproduced: prose on stdout, a clean exit, nothing on stderr.
    let bin = stub_harness(&dir, "echo 'Here is a summary of the repository.'\nexit 0");

    let run = marion_run(&dir, &bin, "Delegate the task to a child.", "30");

    assert_ne!(
        run.code,
        Some(0),
        "the harness exited 0 having called nothing; marion must not pass that on as success.\n\
         stdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stderr.contains("never reached marion's bridge"),
        "the failure must name its cause, not merely be non-zero:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("codex"),
        "and it must name the harness:\n{}",
        run.stderr
    );

    // The prompt really did ride argv — the launch shape this path is built on — and the run was
    // configured, not merely started.
    let argv = std::fs::read_to_string(dir.join("argv.txt")).expect("the stub recorded its argv");
    assert!(
        argv.contains("ARG=Delegate the task to a child."),
        "a LaunchOnly node's prompt is compiled into argv (§6.1 step 8):\n{argv}"
    );
    assert!(
        argv.contains("ARG=exec") && argv.contains("ARG=--json"),
        "{argv}"
    );
    assert!(
        argv.lines().any(|l| l.starts_with("KEY=") && l.len() > 4),
        "codex's generated config names MARION_DUMMY_KEY as its provider env_key; unset, it \
         refuses to start:\n{argv}"
    );
    let codex_home = argv
        .lines()
        .find_map(|l| l.strip_prefix("CODEX_HOME="))
        .expect("CODEX_HOME is recorded");
    assert!(
        Path::new(codex_home).join("config.toml").is_file(),
        "the adapter's config document must have been written before launch: {codex_home}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The same run, one frame different. Without this, the test above would pass against a marion
/// that refused every `LaunchOnly` root unconditionally.
#[test]
fn the_same_root_succeeds_once_one_marion_call_appears_in_its_stream() {
    let dir = scratch("reached");
    let bin = stub_harness(
        &dir,
        &format!("cat <<'EOF'\n{REACHED_THE_BRIDGE}\nEOF\nexit 0"),
    );

    let run = marion_run(&dir, &bin, "Delegate the task to a child.", "30");

    assert_eq!(
        run.code,
        Some(0),
        "one mcp_tool_call for marion is the post-hoc readiness evidence §6.1 asks for\n\
         stdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout.contains("mcp_tool_call"),
        "the root's stream is its result — a root has no contract to return (§9):\n{}",
        run.stdout
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The bound, and the group kill behind it. A harness that never exits is not hypothetical: S13
/// measured opencode doing exactly that on a provider hang, with no backoff ceiling.
#[test]
fn a_root_that_never_exits_is_killed_on_its_bound_and_leaves_its_group_behind_it_dead() {
    let dir = scratch("bound");
    let pids = dir.join("pids");
    // A backgrounded grandchild that outlives its parent's own exit, recording its pid: the class
    // of process a pid-only kill leaves running. `exec` on the wait keeps the stub itself alive.
    let bin = stub_harness(
        &dir,
        &format!(
            "sleep 600 & echo $! >> '{p}'\nsleep 600\n",
            p = pids.display()
        ),
    );

    let run = marion_run(&dir, &bin, "Hang forever.", "2");

    assert!(
        run.elapsed < Duration::from_secs(30),
        "the wall-clock bound did not fire: {:?}",
        run.elapsed
    );
    assert_ne!(run.code, Some(0), "an expired root is not a success");
    assert!(
        run.stderr.contains("wall-clock bound"),
        "the expiry must be reported as an expiry, not misdiagnosed as a missing bridge:\n{}",
        run.stderr
    );

    // Unconditionally clean up before asserting, so a failure here can never leave a `sleep 600`.
    let recorded: Vec<i32> = std::fs::read_to_string(&pids)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    let mut survivors = recorded.clone();
    for _ in 0..40 {
        survivors.retain(|p| {
            Command::new("kill")
                .args(["-0", &p.to_string()])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        });
        if survivors.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    for p in &recorded {
        let _ = Command::new("kill").args(["-9", &p.to_string()]).output();
    }
    assert_eq!(
        recorded.len(),
        1,
        "the stub recorded its grandchild: {recorded:?}"
    );
    assert!(
        survivors.is_empty(),
        "the root's descendants outlived the group kill — the S7 class of failure: {survivors:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
