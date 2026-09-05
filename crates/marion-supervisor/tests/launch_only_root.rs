//! `marion run` on a **`LaunchOnly` root** (design §3.4, §6.1 step 8, §9), end to end through the
//! real `marion` binary, for **all three `LaunchOnly` harnesses**.
//!
//! Three properties, and each one is asserted for codex, gemini and opencode separately:
//!
//! 1. a root whose turn never reached marion's bridge **fails loudly, naming the cause** — it does
//!    not exit 0 having emitted plain text, which §6.1 calls out as the failure the readiness gate
//!    exists for and §12 records happening for real;
//! 2. the same run with one marion tool call in its stream **succeeds** — so property 1 is a check
//!    that can pass, not an assertion that always fires;
//! 3. the wall-clock bound is enforced and the process group is killed. **Not optional**: measured
//!    in S13, opencode never exits on a provider hang (a 500 still retrying at 90 s, a
//!    connection-refused still hung at 180 s, no backoff ceiling), so without a bound
//!    `marion run opencode` is an unbounded hang.
//!
//! # Three roots, nine tests, and never a loop
//!
//! `claude-code` is deliberately absent: it is the **duplex** root path (`root_path` dispatches on
//! `surfaces().control`), where the prompt is a frame written after launch and readiness is gated
//! *before* the turn instead of asserted after it. It has no post-hoc bridge assertion to test here
//! at all. The other three are `launch_only_with_protocol_events()` and take this path.
//!
//! None of the three properties is harness-specific — every one of them is `root::launch_only`
//! behaviour, and that function is the same code for all three — but **the evidence each property
//! rests on is not**. Each harness spells its stream, its argv and its configuration channel its
//! own way:
//!
//! | | argv | one marion call, in-stream | where marion's declaration lands |
//! |---|---|---|---|
//! | codex | `exec --json … <prompt>` | `item.completed` → `mcp_tool_call`, `server: "marion"` | `$CODEX_HOME/config.toml` |
//! | gemini | `-m <model> --output-format stream-json -p <prompt>` | `tool_use` → `tool_name: "mcp_marion_spawn"` | `$GEMINI_CLI_SYSTEM_SETTINGS_PATH` |
//! | opencode | `run --pure --format json --title … -m <model> <prompt>` | `tool_use` → `part.tool: "marion_spawn"` | `$XDG_CONFIG_HOME/opencode/opencode.json` |
//!
//! That is exactly the shape that breaks for one harness while two keep passing, so each is its own
//! `#[test]`, named for its harness. A loop would report the first failure and hide the other two —
//! the line `cross_product.rs` and `depth_gate.rs` take, for the same reason, and the [`Node`] table
//! below is theirs.
//!
//! # Why the harness is a stub and not the real binary
//!
//! What is under test is **marion's** launch path: does it put the prompt in argv, read the
//! stream, assert readiness post hoc, and bound the run? Every one of those is a property of
//! marion given a stream, and a stub harness lets the *stream* be the input — including the two
//! streams no real CLI will produce on demand (one that reaches nothing, and one that never exits).
//!
//! **The stub does not hide the per-harness argv differences; it is how they are measured.** Each
//! stub is named for its own harness (`codex`, `gemini`, `opencode`) and reached through `PATH`, so
//! the argv and env marion compiled are exercised verbatim rather than restated here: the stub
//! records what it was given and the test reads it back, against that harness's own launch shape.
//!
//! What a stub cannot witness is the other half — whether the real CLI *accepts* that argv and that
//! configuration document. `cross_product.rs` covers that for all three of these harnesses as
//! roots, against real binaries, and `m1_hop.rs` for codex end to end.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use marion_core::harness::Harness;
use marion_harness::adapter_for;
use marion_supervisor::run::run_bounded;
use marion_testsupport::scratch;

/// Generous. The bound exists so a hung `marion` fails loudly instead of wedging the suite.
const RUN_BOUND: Duration = Duration::from_secs(60);

/// The `--timeout` every property-3 root is given, as a number the assertions can reason about
/// rather than a literal repeated between the argv and the checks on what it produced.
///
/// **Six seconds and not two, and the extra four are headroom for a race that cannot be closed
/// entirely.** The stub's backgrounded grandchild's pid is the *only* witness property 3 has for
/// the leak it exists to detect, and it is written under marion's wall clock, which starts at
/// `spawn`. Everything between that instant and the stub's first line — `exec`, the shell starting,
/// the script being read — has to fit inside the bound. Normally microseconds; on a machine running
/// several cargo builds, seconds.
///
/// Reproduced rather than inferred: delaying the stub's first line past a 2 s bound fails
/// `a_codex_root_that_never_exits…` with an empty pid set, in a run of **entirely normal
/// duration** — no clock is overrun, so the timing assertions all pass and only the witness is
/// missing. That is the signature of the failures this file saw under load, on codex and on
/// opencode, and all three harnesses run the identical stub and launch path.
///
/// **The bound is the second lever, not the first.** The primary fix is ordering: the pid is
/// recorded in `stub_harness`'s prologue, ahead of the argv write and ahead of the `env`
/// `fork`+`exec` that used to precede it, so the removable part of the window is gone. What is left
/// is irreducible — a process cannot run an instruction before it exists — and this bound covers
/// that remainder.
///
/// It is not a fail-fast bound: `RUN_BOUND` is what keeps a wedged run from wedging the suite, and
/// it is 10x this. The three tests that use this run in parallel, so the cost is ~4 s on the target
/// once, not three times.
const HANG_BOUND: Duration = Duration::from_secs(6);

/// A **starvation sanity ceiling on `HANG_BOUND`, and deliberately not the property.**
///
/// What property 3 is about — the bound fired, and the group died with it — is asserted from
/// marion's own words and from `kill -0`, neither of which a busy machine can perturb. This is the
/// one assertion here that reads a wall clock, and a wall clock includes every second the process
/// spent descheduled. Measured once in this tree, a full-suite run under several concurrent cargo
/// builds took **45 s for a target that takes 2.3 s alone**, and tripped this check at its previous
/// value of 30 s while every load-immune assertion beside it passed.
///
/// **The property underneath that is marion's, not this test's.** `run_bounded_with` deadlines on
/// `Instant`, so time a node spends descheduled or blocked counts against its budget exactly like
/// time it spends working: marion cannot tell a wedged node from a starved one, and a test
/// measuring the same clock from outside inherits that blindness. The evidence to tell them apart
/// already exists and the loop does not read it — the `Drain` threads on the node's stdout and
/// stderr know whether it is still producing output. Changing that would change what
/// `TaskContract.timeout_secs` *means* (a budget of wall time, or of observed progress), so it
/// needs its own justification and is not this file's to make. It is recorded here because it is
/// why a ceiling on this clock can never be both tight and reliable, and why the assertions that
/// carry the property deliberately read something else.
///
/// It is kept, rather than dropped, because it is the only end-to-end witness that the duration
/// marion *enforced* is the duration it was *asked* for: `bin/marion.rs` prints the value it
/// parsed, so a `launch_only` that passed some other duration to `run_bounded` would still say
/// "wall-clock bound" on stderr and still kill the group. Nothing else in the suite covers that
/// hop. It is raised rather than tightened because a check that fires on a busy machine costs more
/// than the narrow class it catches.
const STARVATION_CEILING: Duration = Duration::from_secs(45);

// --- the three LaunchOnly harnesses, as data ----------------------------------------------------

/// One `LaunchOnly` harness, in the only role this file gives it: **root**.
///
/// `claude-code` is not here and must not be added: it is the duplex root path, which gates
/// readiness before the turn rather than asserting it afterwards, and has no `LaunchOnly` launch to
/// test. See the module docs.
#[derive(Debug, Clone, Copy)]
struct Node {
    /// What `marion run <this>` is given. `codex` and `codex-impl` are one built-in; the shorter
    /// spelling is the one the design's prose uses.
    agent_type: &'static str,
    harness: Harness,
    /// The program marion will exec — and therefore the name the stub must take on `PATH`.
    program: &'static str,
    /// `--model`, and what the **compiled** argv must then carry. `None` on codex twice over: its
    /// canned launch compiles no `-m` however loudly one is asked for (`codex::compile_exec`), so
    /// there is nothing to pass and nothing to find.
    model: Option<&'static str>,
}

const CODEX: Node = Node {
    agent_type: "codex",
    harness: Harness::Codex,
    program: "codex",
    model: None,
};

const GEMINI: Node = Node {
    agent_type: "gemini",
    harness: Harness::Gemini,
    program: "gemini",
    // Explicit: the adapter REFUSES to compile without `-m` (S12's `auto` router hang).
    model: Some("gemini-2.5-flash"),
};

const OPENCODE: Node = Node {
    agent_type: "opencode",
    harness: Harness::OpenCode,
    program: "opencode",
    // `provider/model`, the only spelling `-m` accepts; the generated provider block repeats it.
    model: Some("marion/canned-1"),
};

/// One marion tool call in **this harness's own stream shape**, made and **answered**.
///
/// The three shapes are not interchangeable and nothing translates between them: codex names the
/// server and the verb as two fields of an `mcp_tool_call` item, gemini puts the harness-native
/// spelling in `tool_name`, opencode in `part.tool`. The two harness-native spellings are taken
/// from the adapter's own `marion_tool_name` rather than restated, because that mapping *is* the
/// §3.1 contract under test — a frame written by hand here would keep passing if the adapter's
/// spelling changed underneath it.
///
/// **gemini's is two frames and used to be one**, which is not a fixture detail: §6.1 step 8 asks
/// whether a verb was *answered*, and gemini answers a call in a separate `tool_result` frame
/// paired back by `tool_id` (S12's event set, `tests/fixtures/s12/README.md`). A lone `tool_use`
/// was never a complete recording of a working gemini turn; it merely satisfied a gate that asked
/// the weaker question. codex revises its own item in place and opencode emits only terminal
/// states, so those two already carried their verdict in the frame they had.
fn reached_the_bridge(node: &Node) -> String {
    let adapter = adapter_for(node.harness).expect("every harness in this table has an adapter");
    match node.harness {
        Harness::Codex => r#"{"type":"item.completed","item":{"id":"item_0","type":"mcp_tool_call","server":"marion","tool":"spawn","arguments":{},"status":"completed"}}"#.to_string(),
        Harness::Gemini => format!(
            "{}\n{}",
            format_args!(
                r#"{{"type":"tool_use","tool_name":"{}","tool_id":"call-1","args":{{}}}}"#,
                adapter.marion_tool_name("spawn")
            ),
            r#"{"type":"tool_result","tool_id":"call-1","status":"success","output":"ok"}"#,
        ),
        Harness::OpenCode => format!(
            r#"{{"type":"tool_use","part":{{"tool":"{}","state":{{"status":"completed","input":{{}}}}}}}}"#,
            adapter.marion_tool_name("spawn")
        ),
        // Unreachable by construction — the table has three entries and claude-code is not one of
        // them — and a `panic!` rather than a fabricated frame, because a duplex harness arriving
        // here would mean this file had grown a root path it does not test.
        Harness::Copilot => unreachable!("a LaunchOnly root this file does not drive yet"),
        Harness::Acp => unreachable!("not a LaunchOnly root — ACP is Typed(Acp) (§3.4)"),
        Harness::ClaudeCode => panic!(
            "claude-code is the duplex root path and has no LaunchOnly stream to fake (§3.4)"
        ),
    }
}

/// A stub harness on a directory that is prepended to `PATH`, **named for `node`**.
///
/// `body` is shell, run after the stub has recorded its own argv and environment. Returning the
/// directory rather than the file keeps the caller from having to know the program's name — it is
/// the *adapter's* choice, not the test's.
///
/// argv and env go to two files rather than one stream: an environment value is arbitrary text and
/// could contain anything a marker in a shared file might use to separate them.
///
/// # `prologue` runs before the recording, and that ordering is load-bearing
///
/// Everything the stub records is a side effect a *bounded* run may or may not reach: marion's
/// wall clock starts when it spawns this process, and a caller whose test depends on some effect
/// having happened is in a race with that clock. The recording above is not free — `env` is a
/// `fork`+`exec` of another binary, and on a loaded machine that alone can take a noticeable slice
/// of the bound — so anything a test *must* observe goes in `prologue` and runs first, ahead of
/// both writes and the subprocess.
///
/// This is not a cure. A process cannot execute anything before it exists, so the window between
/// marion's `spawn` and this script's first line stays open however it is ordered; `HANG_BOUND`
/// covers that remainder. What `prologue` removes is the part that *is* removable, which is most
/// of it.
fn stub_harness(dir: &Path, node: &Node, prologue: &str, body: &str) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let program = bin.join(node.program);
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\n\
             {prologue}\n\
             for a in \"$@\"; do echo \"ARG=$a\"; done > '{argv}'\n\
             env > '{env}'\n\
             {body}\n",
            argv = dir.join("argv.txt").display(),
            env = dir.join("env.txt").display(),
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

/// The argv marion compiled, in order, as the stub recorded it.
fn recorded_argv(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("argv.txt"))
        .expect("the stub recorded its argv")
        .lines()
        .filter_map(|l| l.strip_prefix("ARG=").map(str::to_string))
        .collect()
}

/// The value of `key` in the environment marion launched the harness with.
///
/// A missing key is the caller's assertion to make, not an empty string: every use below is a
/// channel whose absence is a launch that would have had no configuration at all.
fn recorded_env(dir: &Path, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    std::fs::read_to_string(dir.join("env.txt"))
        .expect("the stub recorded its environment")
        .lines()
        .find_map(|l| l.strip_prefix(&prefix).map(str::to_string))
}

/// The argument **immediately after** `flag`, which is what a flag/value pair means to a CLI. A
/// bare `contains` would accept the value appearing anywhere, including as the prompt's own text.
fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// **The launch shape, per harness.** §6.1 step 8's defining property of this surface is that the
/// prompt is an argv element, and each of the three puts it somewhere different — positionally at
/// the end for codex and opencode, as the argument to `-p` for gemini. The surrounding flags are
/// asserted with it because a prompt that arrived without `--json` / `stream-json` / `--format json`
/// would produce no machine-readable stream for the bridge assertion to read.
fn assert_launched_the_way_this_harness_is_launched(node: &Node, args: &[String], prompt: &str) {
    let h = node.harness;
    match h {
        Harness::Codex => {
            assert_eq!(
                args.first().map(String::as_str),
                Some("exec"),
                "{h}: the non-interactive surface is `codex exec`: {args:?}"
            );
            assert!(
                args.iter().any(|a| a == "--json"),
                "{h}: without --json there is no stream to assert readiness from: {args:?}"
            );
            assert_eq!(
                args.last().map(String::as_str),
                Some(prompt),
                "{h}: the prompt is the trailing positional argument (§6.1 step 8): {args:?}"
            );
        }
        Harness::Gemini => {
            assert_eq!(
                value_of(args, "--output-format"),
                Some("stream-json"),
                "{h}: without stream-json there is no stream to assert readiness from: {args:?}"
            );
            assert_eq!(
                value_of(args, "-p"),
                Some(prompt),
                "{h}: the prompt is the ARGUMENT TO -p — a bare positional launches the \
                 interactive UI and stdin is prepended as context instead: {args:?}"
            );
        }
        Harness::OpenCode => {
            assert_eq!(
                args.first().map(String::as_str),
                Some("run"),
                "{h}: the non-interactive surface is `opencode run`: {args:?}"
            );
            assert_eq!(
                value_of(args, "--format"),
                Some("json"),
                "{h}: without --format json there is no stream to assert readiness from: {args:?}"
            );
            assert!(
                value_of(args, "--title").is_some_and(|t| t.starts_with("marion-")),
                "{h}: without --title opencode issues an extra title-generation request (S13): \
                 {args:?}"
            );
            assert_eq!(
                args.last().map(String::as_str),
                Some(prompt),
                "{h}: the prompt is the trailing positional argument (§6.1 step 8): {args:?}"
            );
        }
        Harness::Copilot => unreachable!("a LaunchOnly root this file does not drive yet"),
        Harness::Acp => unreachable!("not a LaunchOnly root — ACP is Typed(Acp) (§3.4)"),
        Harness::ClaudeCode => unreachable!("not a LaunchOnly root — see the module docs"),
    }
    // The **compiled** model, not the requested one. codex's canned launch compiles no `-m` at all,
    // and asserting its absence is what keeps this from being a check only two harnesses make.
    assert_eq!(
        value_of(args, "-m"),
        node.model,
        "{h}: the compiled argv must carry exactly the model marion put on the wire: {args:?}"
    );
}

/// **Marion's bridge declaration reached this harness by the route its adapter states**, and the
/// document it named exists. Three different channels, each read back from the launch environment
/// rather than rebuilt from a guessed path — a test that recomputed the path would pass against a
/// marion that wrote the file somewhere the harness never looks.
fn assert_the_bridge_declaration_was_written(node: &Node, dir: &Path) {
    let h = node.harness;
    // The variable, and what sits under it. gemini's names the document itself; the other two name
    // a **directory** whose layout beneath is the harness's own — which is why marion creates the
    // parents rather than writing straight into `config_dir`.
    let (var, beneath): (&str, &[&str]) = match h {
        Harness::Codex => ("CODEX_HOME", &["config.toml"]),
        Harness::Gemini => ("GEMINI_CLI_SYSTEM_SETTINGS_PATH", &[]),
        Harness::OpenCode => ("XDG_CONFIG_HOME", &["opencode", "opencode.json"]),
        Harness::Copilot => unreachable!("a LaunchOnly root this file does not drive yet"),
        Harness::Acp => unreachable!("not a LaunchOnly root — ACP is Typed(Acp) (§3.4)"),
        Harness::ClaudeCode => unreachable!("not a LaunchOnly root — see the module docs"),
    };
    let value = recorded_env(dir, var).unwrap_or_else(|| {
        panic!(
            "{h}: ${var} is the channel this harness's configuration arrives by, and the launch \
             environment carries none"
        )
    });
    let mut path = PathBuf::from(&value);
    path.extend(beneath);
    assert!(
        path.is_file(),
        "{h}: the adapter's configuration document must have been written before launch — a node \
         that launches without it has no bridge at all, which is §6.1 step 8's failure and §12's \
         silent one. ${var} = {value}, expected document {}",
        path.display()
    );
    let contents = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{h}: marion wrote {} and it cannot be read back: {e}",
            path.display()
        )
    });
    assert!(
        contents.contains("marion"),
        "{h}: {} names no marion server, so the node would take its turn with no bridge:\n{contents}",
        path.display()
    );
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    elapsed: Duration,
}

/// `marion run <agent-type> --prompt …` with `node`'s stub ahead of any real binary on `PATH`.
fn marion_run(dir: &Path, node: &Node, bin: &Path, prompt: &str, timeout_secs: &str) -> Run {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut args: Vec<String> = vec![
        "run".into(),
        node.agent_type.into(),
        "--prompt".into(),
        prompt.into(),
        "--repo".into(),
        repo.to_string_lossy().into_owned(),
        "--state-dir".into(),
        state.to_string_lossy().into_owned(),
        // Nothing listens here. The stub is the whole model side of this run, and a URL that
        // resolves would only invite a real request. `--canned` is what makes that legible to the
        // binary: a loopback endpoint under real vendor auth is refused at argument parsing,
        // because it aims the operator's credential at a fake server.
        "--canned".into(),
        "--base-url".into(),
        "http://127.0.0.1:9/v1".into(),
        "--timeout".into(),
        timeout_secs.into(),
    ];
    if let Some(m) = node.model {
        args.push("--model".into());
        args.push(m.into());
    }
    let started = Instant::now();
    let out = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_marion"))
            .args(&args)
            .env("PATH", path)
            .current_dir(dir),
        RUN_BOUND,
    )
    .expect("marion run starts");
    assert!(
        !out.timed_out,
        "{}: marion run did not finish inside {RUN_BOUND:?} — it is the thing under test that is \
         supposed to be bounded\nstderr:\n{}",
        node.harness,
        String::from_utf8_lossy(&out.stderr)
    );
    Run {
        code: out.code,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        elapsed: started.elapsed(),
    }
}

// --- property 1: a root that reached nothing fails loudly ---------------------------------------
//
// **The property this whole path exists for.** A `LaunchOnly` root that never called a marion tool
// took its turn without marion's tools; §6.1 forbids letting that end as plain text, because a
// toolless turn exits 0 with no diagnostic anywhere and reads exactly like success.

fn a_silent_root_is_refused(node: &Node, name: &str) {
    let dir = scratch(&format!("lo-{name}"));
    // The measured failure shape, reproduced: prose on stdout, a clean exit, nothing on stderr.
    let bin = stub_harness(
        &dir,
        node,
        // Nothing to record ahead of the argv this test reads: an unbounded run reaches every line.
        "",
        "echo 'Here is a summary of the repository.'\nexit 0",
    );
    let prompt = "Delegate the task to a child.";

    let run = marion_run(&dir, node, &bin, prompt, "30");

    assert_ne!(
        run.code,
        Some(0),
        "{}: the harness exited 0 having called nothing; marion must not pass that on as \
         success.\nstdout:\n{}\nstderr:\n{}",
        node.harness,
        run.stdout,
        run.stderr
    );
    assert!(
        run.stderr.contains("never reached marion's bridge"),
        "{}: the failure must name its cause, not merely be non-zero:\n{}",
        node.harness,
        run.stderr
    );
    assert!(
        run.stderr.contains(node.harness.as_str()),
        "{}: and it must name the harness — a refusal that names none is a refusal an operator \
         running three roots cannot act on:\n{}",
        node.harness,
        run.stderr
    );

    // The prompt really did ride argv, in this harness's own launch shape, and the run was
    // configured rather than merely started.
    assert_launched_the_way_this_harness_is_launched(node, &recorded_argv(&dir), prompt);
    assert_the_bridge_declaration_was_written(node, &dir);
    // Pushed onto every canned `LaunchOnly` root by `root::launch_only`, not by the adapter: codex's
    // generated config names `MARION_DUMMY_KEY` as its provider `env_key` and a provider whose key
    // is unset refuses to start. Asserted for all three because it is marion's launch that sets it,
    // so a marion that dropped it would break codex alone and silently.
    assert!(
        recorded_env(&dir, "MARION_DUMMY_KEY").is_some_and(|v| !v.is_empty()),
        "{}: marion pushes a per-run MARION_DUMMY_KEY onto every canned LaunchOnly root",
        node.harness
    );
}

#[test]
fn a_codex_root_that_never_reached_the_bridge_fails_loudly_instead_of_exiting_zero() {
    a_silent_root_is_refused(&CODEX, "silent-codex");
}

#[test]
fn a_gemini_root_that_never_reached_the_bridge_fails_loudly_instead_of_exiting_zero() {
    a_silent_root_is_refused(&GEMINI, "silent-gemini");
}

#[test]
fn an_opencode_root_that_never_reached_the_bridge_fails_loudly_instead_of_exiting_zero() {
    a_silent_root_is_refused(&OPENCODE, "silent-opencode");
}

// --- property 2: one marion call is enough ------------------------------------------------------
//
// The same run, one frame different. Without this, property 1 would pass against a marion that
// refused every `LaunchOnly` root unconditionally.

fn a_root_that_called_marion_succeeds(node: &Node, name: &str) {
    let frame = reached_the_bridge(node);
    // The fixture holds itself to the adapter's own reading first. A frame this harness's parser
    // does not recognise would fail the run below for the *fixture's* reason while looking exactly
    // like marion failing to read a real stream.
    let adapter = adapter_for(node.harness).expect("every harness in this table has an adapter");
    assert_eq!(
        adapter.marion_tool_calls(&frame),
        vec!["spawn".to_string()],
        "{}: this file's `reached_the_bridge` frame must be one the harness's OWN adapter reads as \
         a marion call, or the run below would fail for the fixture's reason:\n{frame}",
        node.harness
    );

    let dir = scratch(&format!("lo-{name}"));
    let bin = stub_harness(
        &dir,
        node,
        "",
        &format!("cat <<'EOF'\n{frame}\nEOF\nexit 0"),
    );

    let run = marion_run(&dir, node, &bin, "Delegate the task to a child.", "30");

    assert_eq!(
        run.code,
        Some(0),
        "{}: one marion tool call is the post-hoc readiness evidence §6.1 asks for\nstdout:\n{}\n\
         stderr:\n{}",
        node.harness,
        run.stdout,
        run.stderr
    );
    assert_eq!(
        adapter.marion_tool_calls(&run.stdout),
        vec!["spawn".to_string()],
        "{}: the root's stream is its result — a root has no contract to return (§9) — so the \
         frame it emitted must survive onto marion's own stdout, still readable as the marion call \
         it was:\n{}",
        node.harness,
        run.stdout
    );
}

#[test]
fn a_codex_root_succeeds_once_one_marion_call_appears_in_its_stream() {
    a_root_that_called_marion_succeeds(&CODEX, "reached-codex");
}

#[test]
fn a_gemini_root_succeeds_once_one_marion_call_appears_in_its_stream() {
    a_root_that_called_marion_succeeds(&GEMINI, "reached-gemini");
}

#[test]
fn an_opencode_root_succeeds_once_one_marion_call_appears_in_its_stream() {
    a_root_that_called_marion_succeeds(&OPENCODE, "reached-opencode");
}

// --- property 2b: a call that was refused is not a call that was answered -----------------------
//
// The gap between properties 1 and 2. A root can reach marion's bridge and still have delegated
// nothing — the call goes out and comes back an error, and the harness carries on and exits 0.
// Measured, not hypothesised: `tasks/todo.md`'s owed item 0 records a gemini root whose `spawn` was
// refused by gemini's own schema validator, after which the root finished its turn, exited 0, and
// marion journalled `ExitStatus::Ok` for a run that delegated nothing. Property 2's frame and this
// one differ in exactly one field, which is the whole point: an *attempted* verb is not evidence.

/// The same one marion call as [`reached_the_bridge`], in the same harness-native shape, **refused**.
///
/// One of the three is a recorded shape and two are constructed, and the difference is stated
/// rather than smoothed over:
///
/// * **opencode is measured.** S13 recorded `{"status":"error","error":"The user rejected
///   permission to use this specific tool call."}` on the tool part, with the run continuing and
///   exiting 0 — `opencode::parse_stream` has read that shape since S13.
/// * **codex is constructed.** `tests/fixtures/s6/` records the `mcp_tool_call` item carrying
///   `result`, `error` and `status`, but only ever with `status: "completed"`, `error: null`. The
///   failed spelling here is the obvious complement of the recorded success and is **not** a
///   capture of a real refusal.
/// * **gemini is constructed.** S12 records the `tool_result` frame and its `status`, but captured
///   only `"success"`; it also redacted the `tool_id` on the `tool_use` and on the `tool_result`
///   differently, so even the pairing of a result to its call is an assumption about that stream,
///   not something the fixture proves.
fn refused_at_the_bridge(node: &Node) -> String {
    let adapter = adapter_for(node.harness).expect("every harness in this table has an adapter");
    match node.harness {
        Harness::Codex => r#"{"type":"item.completed","item":{"id":"item_0","type":"mcp_tool_call","server":"marion","tool":"spawn","arguments":{},"result":null,"error":"spawn is not permitted here","status":"failed"}}"#.to_string(),
        Harness::Gemini => format!(
            "{}\n{}",
            format_args!(
                r#"{{"type":"tool_use","tool_name":"{}","tool_id":"call-1","parameters":{{}}}}"#,
                adapter.marion_tool_name("spawn")
            ),
            r#"{"type":"tool_result","tool_id":"call-1","status":"error","output":"invalid arguments"}"#,
        ),
        Harness::OpenCode => format!(
            r#"{{"type":"tool_use","part":{{"tool":"{}","state":{{"status":"error","error":"The user rejected permission to use this specific tool call."}}}}}}"#,
            adapter.marion_tool_name("spawn")
        ),
        Harness::Copilot => unreachable!("a LaunchOnly root this file does not drive yet"),
        Harness::Acp => unreachable!("not a LaunchOnly root — ACP is Typed(Acp) (§3.4)"),
        Harness::ClaudeCode => panic!(
            "claude-code is the duplex root path and has no LaunchOnly stream to fake (§3.4)"
        ),
    }
}

fn a_root_whose_only_call_was_refused_is_not_a_success(node: &Node, name: &str) {
    let frame = refused_at_the_bridge(node);
    let dir = scratch(&format!("lo-{name}"));
    let bin = stub_harness(
        &dir,
        node,
        "",
        &format!("cat <<'EOF'\n{frame}\nEOF\nexit 0"),
    );

    let run = marion_run(&dir, node, &bin, "Delegate the task to a child.", "30");

    assert_ne!(
        run.code,
        Some(0),
        "{}: the root's only marion call was refused, so it delegated nothing; marion must not \
         pass that on as success.\nstdout:\n{}\nstderr:\n{}",
        node.harness,
        run.stdout,
        run.stderr
    );
    assert!(
        run.stderr.contains("was answered"),
        "{}: the failure must name *this* cause — the bridge was reached and nothing came back \
         from it — and not the different one property 1 asserts:\n{}",
        node.harness,
        run.stderr
    );
    assert!(
        !run.stderr.contains("never reached marion's bridge"),
        "{}: a root whose call was refused DID reach the bridge; blaming a missing bridge would \
         send an operator to the wrong fix:\n{}",
        node.harness,
        run.stderr
    );
    assert!(
        run.stderr.contains(node.harness.as_str()),
        "{}: and it must name the harness:\n{}",
        node.harness,
        run.stderr
    );
}

#[test]
fn a_codex_root_whose_only_marion_call_was_refused_is_not_a_success() {
    a_root_whose_only_call_was_refused_is_not_a_success(&CODEX, "refused-codex");
}

#[test]
fn a_gemini_root_whose_only_marion_call_was_refused_is_not_a_success() {
    a_root_whose_only_call_was_refused_is_not_a_success(&GEMINI, "refused-gemini");
}

#[test]
fn an_opencode_root_whose_only_marion_call_was_refused_is_not_a_success() {
    a_root_whose_only_call_was_refused_is_not_a_success(&OPENCODE, "refused-opencode");
}

// --- property 3: the bound, and the group kill behind it ----------------------------------------
//
// A harness that never exits is not hypothetical: S13 measured opencode doing exactly that on a
// provider hang, with no backoff ceiling.

fn a_hanging_root_is_killed_with_its_group(node: &Node, name: &str) {
    let dir = scratch(&format!("lo-{name}"));
    let pids = dir.join("pids");
    // A backgrounded grandchild that outlives its parent's own exit, recording its pid: the class
    // of process a pid-only kill leaves running.
    //
    // **In the prologue, so it is the stub's first act.** That pid file is the only witness this
    // test has for the leak below, and it is written under marion's wall clock: every instruction
    // ahead of it is time in which the group can be killed with nothing recorded, which fails the
    // run on a machine that was merely slow. `sh` writes a redirected builtin immediately — there
    // is no buffering to flush — so first is as early as it can be made. See `stub_harness`.
    let bin = stub_harness(
        &dir,
        node,
        &format!("sleep 600 & echo $! >> '{p}'", p = pids.display()),
        "sleep 600\n",
    );

    let run = marion_run(
        &dir,
        node,
        &bin,
        "Hang forever.",
        &HANG_BOUND.as_secs().to_string(),
    );

    // ---- the property: the bound fired, in marion's own words. ----------------------------------
    //
    // Asserted first and read from the run's own report rather than from a clock. A bound that
    // never fired at all does not reach here: the stub sleeps 600 s, so `marion_run`'s outer
    // `RUN_BOUND` would have caught it, saying that the thing under test is the thing that was
    // supposed to be bounded.
    assert_ne!(
        run.code,
        Some(0),
        "{}: an expired root is not a success",
        node.harness
    );
    assert!(
        run.stderr.contains("wall-clock bound"),
        "{}: the expiry must be reported as an expiry, not misdiagnosed as a missing bridge:\n{}",
        node.harness,
        run.stderr
    );

    // ---- the bound was not cut short. ------------------------------------------------------------
    //
    // **The starvation-immune half**, and the reason it is worth stating separately: a busy machine
    // can only make `elapsed` larger, never smaller, so this direction means the same thing on an
    // idle box and a loaded one. It fails a root that returned before the bound it was given — a
    // `--timeout` parsed wrong, or ignored in favour of some shorter constant.
    assert!(
        run.elapsed >= HANG_BOUND,
        "{}: the root came back in {:?}, sooner than the {:?} it was given, so the duration marion \
         enforced is not the one it was asked for.\nstderr:\n{}",
        node.harness,
        run.elapsed,
        HANG_BOUND,
        run.stderr
    );
    assert!(
        run.elapsed < STARVATION_CEILING,
        "{}: the bound FIRED — stderr above says so, and this assertion is downstream of that — \
         but the run took {:?} against a requested {:?}, over this file's {:?} ceiling.\n\
         Two things produce that, and they are told apart by what else failed:\n\
         - if every other assertion in this test passed, the likely cause is the machine rather \
         than marion. `run_bounded` deadlines on `Instant`, which counts time the process spent \
         descheduled, so a loaded box inflates this number without anything being wrong. Measured \
         in this tree: a full suite under concurrent cargo builds took 45 s for a target that \
         takes 2.3 s alone. Re-run it alone before believing it.\n\
         - if it reproduces on an idle machine, marion enforced a longer bound than it was asked \
         for — `root::launch_only` passing something other than its `bound` to `run_bounded` is \
         the shape, and this is the only test in the suite that would notice.\n\
         stderr:\n{}",
        node.harness,
        run.elapsed,
        HANG_BOUND,
        STARVATION_CEILING,
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
            // `kill -0` is the *only* witness this test has for a leak. A non-zero exit is its
            // real answer — the process is gone — but a `kill` that would not run at all is no
            // answer, and reading that as "dead" would make the leak assertion below pass for
            // free on exactly the machines where it cannot be checked.
            Command::new("kill")
                .args(["-0", &p.to_string()])
                .output()
                .expect(
                    "`kill -0` must run: without it nothing here can tell a clean run from a leak",
                )
                .status
                .success()
        });
        if survivors.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    for p in &recorded {
        let _ = Command::new("kill").args(["-9", &p.to_string()]).output();
    }
    // **The leak check's witness, asserted before the leak check reads it.** An empty set here is
    // not a clean run and must never be read as one — it is a run whose only evidence never
    // existed, so it fails rather than passing vacuously.
    //
    // The two ways it can be empty are told apart rather than merged, by a marker the stub writes
    // *after* the pid: `argv.txt`. Reaching that file means the stub was running and the pid line
    // ran before it, so a missing pid with a present argv is a defect in the recording itself,
    // while neither file means the stub never got a first instruction at all.
    let stub_started = dir.join("argv.txt").is_file();
    assert_eq!(
        recorded.len(),
        1,
        "{}: no grandchild pid was recorded, so the leak assertion below has no witness and this \
         run proves nothing either way — it is NOT evidence that the group kill worked.\n\
         The stub reached its later `argv.txt` write: {stub_started}.\n\
         - `false` — the stub never ran a first instruction inside the {:?} it was given. marion's \
         bound is a wall clock that starts at `spawn`, and `exec` plus shell start-up on a loaded \
         machine can exceed it; nothing the stub does can precede its own creation, which is why \
         `HANG_BOUND` carries headroom and the pid is recorded in the stub's prologue. Re-run \
         alone before believing it.\n\
         - `true` — the stub WAS running and still recorded nothing, which the ordering is \
         supposed to make impossible. That is a real defect in this test's witness, not a slow \
         machine.\n\
         recorded: {recorded:?}",
        node.harness,
        HANG_BOUND
    );
    assert!(
        survivors.is_empty(),
        "{}: the root's descendants outlived the group kill — the S7 class of failure: {survivors:?}",
        node.harness
    );
}

#[test]
fn a_codex_root_that_never_exits_is_killed_on_its_bound_and_leaves_its_group_behind_it_dead() {
    a_hanging_root_is_killed_with_its_group(&CODEX, "bound-codex");
}

#[test]
fn a_gemini_root_that_never_exits_is_killed_on_its_bound_and_leaves_its_group_behind_it_dead() {
    a_hanging_root_is_killed_with_its_group(&GEMINI, "bound-gemini");
}

#[test]
fn an_opencode_root_that_never_exits_is_killed_on_its_bound_and_leaves_its_group_behind_it_dead() {
    a_hanging_root_is_killed_with_its_group(&OPENCODE, "bound-opencode");
}
