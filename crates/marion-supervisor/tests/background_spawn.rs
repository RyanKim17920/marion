//! **`spawn { background: true }` really is not synchronous** — and the other three things that
//! only became true, or only became false, on the day it stopped being.
//!
//! Driven through the **real `marion-supervisor mcp` binary over stdio**, the way a harness drives
//! it, because `background` is read in `main::handle_tool_call` and a library-level test would
//! assert about `Background` rather than about the verb a model actually calls.
//!
//! # The child is a shim, and that is the point
//!
//! Every test here puts a `codex` shim first on `PATH`. The shim is a real process that marion
//! really launches, really waits on and really reaps — what it is not is a language model. It
//! **blocks until the test creates a gate file**, and that is the entire reason this file can make
//! an ordering claim at all:
//!
//! > if `spawn` were synchronous, the reply to the `spawn` frame could not arrive until the child
//! > exited, and the child cannot exit until the test — which is blocked reading that reply —
//! > creates the gate file. A synchronous implementation **deadlocks**.
//!
//! No elapsed time is measured anywhere and no assertion says "this was fast". The bound in
//! [`read_reply`] exists solely so a deadlock is a loud failure naming the deadlock instead of a
//! suite that hangs, which is `duplex.rs`'s self-destruct idiom applied to a test.
//!
//! **Why a shim rather than a real `codex` against the canned provider.** A real child finishes on
//! its own schedule, so every assertion about *"the caller is here while the child is there"* would
//! be a race the test happened to win. The shim makes the child's terminal transition an event the
//! test causes, which is the difference between an ordering assertion and a timing one.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test background_spawn
//! ```
//!
//! It needs **no harness binary, no network and no credential**: the shim replaces `codex` and
//! nothing here reaches a model.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use marion_testsupport::{Liveness, Scratch, fixture_repo, liveness, scratch};
use serde_json::{Value, json};

/// A bound that exists **only to fail**, never to be reached on a passing run.
///
/// Every reply this file waits for is produced by a bridge that is not waiting on anything the
/// test has not already done, so the passing path takes milliseconds. Reaching this bound means
/// the bridge is blocked — which, for the ordering control, is precisely the bug under test. It is
/// therefore deliberately generous: a bound tight enough to be a *timing* assertion would turn a
/// slow machine into a false failure, and this bound is never the thing being measured.
///
/// **It must never be widened to fix a flake.** If it is ever reached, something is genuinely
/// blocked and the fix is upstream of this file.
const DEADLOCK_BOUND: Duration = Duration::from_secs(60);

/// What the shim child is given as its wall clock. Long enough that it never expires on the
/// passing path — the gate file, not this number, is what ends every child here.
const CHILD_TIMEOUT_SECS: u64 = 120;

/// The shim's own hard lifetime, so no bad run can strand one.
///
/// Comfortably longer than any passing test's gate-to-exit window and comfortably shorter than
/// [`CHILD_TIMEOUT_SECS`], so it is never the thing that ends a child on a passing run and always
/// the thing that ends one on a failing run.
const SHIM_LIFE: Duration = Duration::from_secs(90);

/// How long the EOF-hold control samples for an early departure.
///
/// **Not a correctness bound.** A bridge that drops the hold exits within milliseconds of EOF, so
/// this only has to be longer than a process teardown; a machine slow enough to exceed it takes
/// the test's other branch and still passes. It is deliberately *not* named a timeout: nothing
/// fails because this elapsed.
const EOF_SAMPLE: Duration = Duration::from_secs(3);

/// A `codex` that blocks until the test says otherwise.
///
/// * `--version` answers immediately — unless the `slow_version` marker exists, which is how one
///   test reproduces a harness that answers its run and then hangs on the version probe.
/// * Anything else writes a *started* marker — which is what lets a test assert a real process
///   existed, rather than inferring it — then polls for the gate file and exits 0.
/// * Anything else writes a *started* marker — which is what lets a test assert a real process
///   existed, rather than inferring it — then polls for the gate file and exits 0.
///
/// It writes nothing to stdout, so the child's contract lands `Unreported`. That is correct and
/// irrelevant: this file asserts about *when* contracts arrive and *whether* spawns are refused,
/// never about what a child said.
fn shim(
    dir: &Path,
    gate: &Path,
    started_dir: &Path,
    done_dir: &Path,
    slow_version: &Path,
) -> PathBuf {
    let bin = dir.join("codex");
    let script = format!(
        r#"#!/bin/sh
case "$1" in
  --version)
    # **A harness that hangs on `--version`.** Opt-in, because every other test in this file needs
    # the probe to be instant — which, since §11 item 28 step 1 moved the probe ahead of the launch
    # (§6.1 step 3), is now a claim about the *pre-launch* path rather than about a caller waiting
    # after its child is done. The child's `timeout_secs` bounds the run and never this, so an
    # unbounded probe is an unbounded `run_spawn` either way, and on the background path a `wait`
    # that never returns.
    #
    # It sleeps rather than waiting for the gate, so the hang does not depend on the gate's state
    # in either ordering; it only has to outlast marion's own probe bound to be a hang at all. The
    # sleep is the shim's own life cap, so no bad run strands it indefinitely.
    if [ -e {slow_version} ]; then sleep {life_secs}; fi
    echo "codex-cli 0.146.0-marion-shim"; exit 0 ;;
esac
mkdir -p {started} {done}
# A distinct marker per invocation, so a test can count the children that really launched.
: > {started}/$$
# **Mortal by construction.** The gate lives in a `Scratch` directory that is removed when the
# test's fixture drops, so a shim that only ever waited for the gate would spin forever if the
# test panicked, or if the bridge holding it was killed by `Bridge::drop` — which is exactly what
# a mutation run does. One was found stranded that way on 2026-08-06. This is S16's own lesson
# applied to a test fixture: a probe that cannot die on its own is a leak waiting for a bad run,
# and the cap must be shorter than the child's contract timeout so it is never what ends a
# passing run.
waited=0
while [ ! -e {gate} ]; do
  sleep 0.05
  waited=$((waited + 1))
  if [ "$waited" -gt {life_ticks} ]; then
    exit 0
  fi
done
# And a second marker at exit, so "the child has finished" is an event the test can observe
# rather than a time it has to guess. The EOF-hold control is an ordering claim between this
# marker and the bridge's own exit.
: > {done}/$$
exit 0
"#,
        started = shell_quote(started_dir),
        done = shell_quote(done_dir),
        gate = shell_quote(gate),
        slow_version = shell_quote(slow_version),
        life_ticks = SHIM_LIFE.as_millis() / 50,
        life_secs = SHIM_LIFE.as_secs(),
    );
    std::fs::write(&bin, script).expect("the shim is written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
            .expect("the shim is executable");
    }
    bin
}

fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"))
}

/// One bridge process, driven the way a harness drives it.
struct Bridge {
    child: Child,
    /// `Option` so stdin can be closed *without* consuming the `Bridge` — the EOF hold test has to
    /// close it and then keep observing the process, and `Drop`'s cleanup must survive a panic in
    /// between.
    stdin: Option<ChildStdin>,
    /// Lines, read by a thread, so [`DEADLOCK_BOUND`] can actually bound the wait.
    ///
    /// A `BufReader::read_line` on a pipe blocks uninterruptibly, so a deadline checked around it
    /// is decorative — it is only consulted once the read has already returned. Moving the read
    /// onto its own thread and waiting on a channel is what makes the bound real, and the bound
    /// being real is the whole negative control: a synchronous `spawn` must fail *naming the
    /// deadlock*, not by whatever happens after the child's own wall clock expires minutes later.
    lines: Receiver<Option<String>>,
    next_id: i64,
}

impl Bridge {
    /// Start `marion-supervisor mcp` with the declaration marion would have written for a **root**
    /// (`MARION_AGENT_ID` / `MARION_AGENT_TYPE` / `MARION_DEPTH`), plus the shim ahead of `codex`
    /// on `PATH`.
    ///
    /// The environment is set on the child process, never on the test process: `PATH` is global
    /// state and a test that mutated its own would leak the shim into every other test in this
    /// binary, including ones that expect a real harness.
    fn start(repo: &Path, state: &Path, shim_dir: &Path) -> Self {
        Self::start_with_path(
            repo,
            state,
            &format!(
                "{}:{}",
                shim_dir.to_string_lossy(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
    }

    /// [`Bridge::start`] with the `PATH` stated outright, for the one test that needs to control
    /// what marion can and cannot find.
    ///
    /// **`execvp` does not stop at the first match.** A `PATH` entry whose `codex` fails to exec is
    /// skipped and the search continues, so putting a deliberately unlaunchable shim first proves
    /// nothing while a real `codex` sits further along — which is exactly what this file's own
    /// fixture did on the first run of that test, launching the machine's real harness. The only
    /// way to make "the harness cannot be launched" a fact rather than a hope is to state the whole
    /// search path.
    fn start_with_path(repo: &Path, state: &Path, path: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_marion-supervisor"))
            .arg("mcp")
            .env("PATH", path)
            .env("MARION_REPO", repo)
            .env("MARION_STATE_DIR", state)
            .env("MARION_AGENT_ID", "root-background-spawn")
            .env("MARION_AGENT_TYPE", "claude")
            .env("MARION_DEPTH", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the bridge binary starts");
        let stdin = Some(child.stdin.take().expect("piped"));
        let mut stdout = BufReader::new(child.stdout.take().expect("piped"));
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            loop {
                let mut line = String::new();
                match stdout.read_line(&mut line) {
                    // EOF: `None` so a waiter learns the bridge closed rather than timing out.
                    Ok(0) => {
                        let _ = tx.send(None);
                        return;
                    }
                    Ok(_) => {
                        if !line.trim().is_empty() && tx.send(Some(line)).is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(None);
                        return;
                    }
                }
            }
        });
        let mut b = Self {
            child,
            stdin,
            lines,
            next_id: 1,
        };
        b.call("initialize", json!({}));
        b.call("tools/list", json!({}));
        b
    }

    /// Send a request and read its reply, failing loudly on a deadlock rather than hanging.
    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let w = self.stdin.as_mut().expect("stdin is still open");
        writeln!(w, "{frame}").expect("the bridge is still reading");
        w.flush().expect("flushed");
        self.read_reply(method)
    }

    /// Ask for a tool by name.
    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.call("tools/call", json!({"name": name, "arguments": arguments}))
    }

    /// The bridge answers one frame per line and answers every frame it is sent, so a reply is
    /// exactly the next line — **or [`DEADLOCK_BOUND`] elapses and this panics naming the frame it
    /// was waiting for.** That panic is the negative control for backgrounding: with a synchronous
    /// `spawn` the reply cannot arrive until the child exits, and the child cannot exit until a
    /// gate this test has not reached yet.
    fn read_reply(&mut self, what: &str) -> Value {
        match self.lines.recv_timeout(DEADLOCK_BOUND) {
            Ok(Some(line)) => serde_json::from_str(line.trim()).unwrap_or_else(|e| {
                panic!("the bridge's reply to {what} is not json: {e}: {line}")
            }),
            Ok(None) => panic!(
                "the bridge closed its stdout without answering {what}; its stderr is inherited"
            ),
            Err(RecvTimeoutError::Timeout) => panic!(
                "DEADLOCK: no reply to {what} within the bound. If this is the `spawn` frame, the \
                 spawn is behaving synchronously — it is waiting for a child that is waiting for a \
                 gate this test opens only after the reply arrives."
            ),
            Err(RecvTimeoutError::Disconnected) => {
                panic!("the bridge's reader thread is gone before answering {what}")
            }
        }
    }

    /// EOF: what a *well-behaved* client's departure looks like to the bridge. `s16` measured that
    /// a real Claude Code harness does **not** do this — see the module note on that test.
    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// The bridge's **own** pid — the process that calls `run_spawn`, not the process it starts.
    ///
    /// Held only so the pid assertion can name the mutation it rules out: a `Spawned` record
    /// filled in with `std::process::id()` names a live process and would satisfy any assertion
    /// that only asked for liveness.
    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    /// Close stdin and wait for the bridge to leave.
    fn close(mut self) -> std::process::ExitStatus {
        self.close_stdin();
        self.child.wait().expect("the bridge exits on EOF")
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // A test that panicked mid-conversation must not leave a bridge — or the shim children it
        // is holding — behind. `kill` is unconditional and its error ignored: the process may
        // already be gone, which is the outcome this wants.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text_of(reply: &Value) -> String {
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn is_error(reply: &Value) -> bool {
    reply["result"]["isError"] == json!(true)
}

/// The `task_id` a handle carries, which is the only thing `wait` can be addressed with.
fn handle_task_id(reply: &Value) -> String {
    let text = text_of(reply);
    let at = text
        .find("task_id \"")
        .unwrap_or_else(|| panic!("a handle names its task_id: {text}"));
    let rest = &text[at + "task_id \"".len()..];
    rest[..rest.find('"').expect("the id is quoted")].to_string()
}

fn spawn_args(background: bool) -> Value {
    spawn_args_with_timeout(background, CHILD_TIMEOUT_SECS)
}

/// The same spawn with the wall clock spelled out, so a test can hand marion a number the caller
/// controls and marion must survive.
fn spawn_args_with_timeout(background: bool, timeout_secs: u64) -> Value {
    json!({
        "agent_type": "codex-impl",
        "prompt": "block until the gate opens",
        "acceptance_criteria": ["the shim exits 0"],
        "timeout_secs": timeout_secs,
        "background": background,
    })
}

struct Fixture {
    _scratch: Scratch,
    repo: PathBuf,
    state: PathBuf,
    shim_dir: PathBuf,
    gate: PathBuf,
    started: PathBuf,
    done: PathBuf,
    slow_version: PathBuf,
}

fn fixture(tag: &str) -> Fixture {
    let s = scratch(tag);
    let root = s.to_path_buf();
    let repo = fixture_repo(&root);
    let state = root.join("state");
    let shim_dir = root.join("bin");
    let started = root.join("started");
    let done = root.join("done");
    let gate = root.join("gate");
    let slow_version = root.join("slow-version");
    std::fs::create_dir_all(&shim_dir).expect("the shim dir");
    std::fs::create_dir_all(&state).expect("the state dir");
    shim(&shim_dir, &gate, &started, &done, &slow_version);
    Fixture {
        _scratch: s,
        repo,
        state,
        shim_dir,
        gate,
        started,
        done,
        slow_version,
    }
}

impl Fixture {
    fn bridge(&self) -> Bridge {
        Bridge::start(&self.repo, &self.state, &self.shim_dir)
    }
    /// A bridge whose **only** `codex` is a deliberately unlaunchable one.
    ///
    /// The shim is rewritten with an interpreter that does not exist, so it is still found and
    /// still executable and the failure lands inside `command.spawn()` — and the `PATH` is cut
    /// down to the system directories, because `execvp` skips a match that fails to exec and keeps
    /// searching. With the ordinary `PATH` this test launched the machine's real `codex` instead,
    /// which is how that was discovered rather than reasoned about.
    ///
    /// The two system directories are asserted to hold no `codex` rather than assumed to, so the
    /// test states its own precondition instead of depending on how the machine was set up. `git`
    /// and `ps` — the only other programs the spawn path shells out to — live in them.
    fn bridge_with_an_unlaunchable_harness(&self) -> Bridge {
        let bin = self.shim_dir.join("codex");
        std::fs::write(&bin, "#!/marion/no/such/interpreter\nexit 0\n")
            .expect("the broken shim is written");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
            .expect("the broken shim is executable");
        for dir in ["/usr/bin", "/bin"] {
            assert!(
                !Path::new(dir).join("codex").exists(),
                "{dir} holds a `codex`, so the reduced PATH below would let marion launch a real \
                 harness and this test would assert nothing"
            );
        }
        Bridge::start_with_path(
            &self.repo,
            &self.state,
            &format!("{}:/usr/bin:/bin", self.shim_dir.to_string_lossy()),
        )
    }

    /// Make the shim hang when asked its version, from the next invocation on.
    fn make_version_slow(&self) {
        std::fs::write(&self.slow_version, b"hang").expect("the slow-version marker is written");
    }
    /// Release every blocked shim child.
    fn open_gate(&self) {
        std::fs::write(&self.gate, b"go").expect("the gate opens");
    }
    /// How many shim children have actually started. A *measurement* of processes marion launched,
    /// not an inference from marion's own bookkeeping — the S7 lesson is that a check which reads
    /// only the thing under test cannot see it lying.
    fn started_children(&self) -> usize {
        std::fs::read_dir(&self.started)
            .map(|d| d.flatten().count())
            .unwrap_or(0)
    }
    /// The pids of the shim children that have actually started, **as the children named
    /// themselves**.
    ///
    /// The shim's marker file is named `$$` — the started process's own pid, written by that
    /// process. So this is the one measurement in the file that can contradict marion about
    /// *which* process it started, rather than only about how many. Every other route to a pid
    /// here would be marion's own bookkeeping restated.
    fn started_pids(&self) -> Vec<i32> {
        let mut pids: Vec<i32> = std::fs::read_dir(&self.started)
            .map(|d| {
                d.flatten()
                    .filter_map(|e| e.file_name().to_string_lossy().parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        pids.sort_unstable();
        pids
    }

    /// How many shim children have actually *exited*. The other end of the same measurement, and
    /// the one the EOF-hold control orders the bridge's own exit against.
    fn finished_children(&self) -> usize {
        std::fs::read_dir(&self.done)
            .map(|d| d.flatten().count())
            .unwrap_or(0)
    }

    /// Every `journal.jsonl` under this fixture's state dir, concatenated.
    ///
    /// The project hash that names the directory is derived inside the bridge, so the path is
    /// discovered rather than reconstructed — a test that recomputed the hash would be asserting
    /// against its own copy of the derivation instead of against the file marion wrote.
    fn journal_text(&self) -> String {
        fn walk(dir: &Path, out: &mut String) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.file_name().is_some_and(|n| n == "journal.jsonl") {
                    out.push_str(&std::fs::read_to_string(&p).unwrap_or_default());
                }
            }
        }
        let mut out = String::new();
        walk(&self.state, &mut out);
        out
    }
}

/// The `pid` field of every `Spawned` record in a journal, in order.
///
/// `Option<i32>` and not `i32`, because the two absences this file has to keep apart are *"no
/// `Spawned` record has been written yet"* (an empty vector) and *"a `Spawned` record was written
/// and records no pid"* (a `None` element). Collapsing them would let the record's own regression
/// — going back to `pid: None` — read as a record that had not arrived yet, which is the one
/// mutation the pid assertion exists to catch.
fn spawned_pids(journal: &str) -> Vec<Option<i32>> {
    journal
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            v.get("kind")
                .and_then(|k| k.get("Spawned"))
                .map(|s| s.get("pid").and_then(Value::as_i64).map(|p| p as i32))
        })
        .collect()
}

/// **THE ordering control: the caller regains the turn while its child is still running.**
///
/// This is the assertion the whole change exists to make true, and it is stated as an ordering,
/// never as a duration. The sequence, in the order the test forces it to happen:
///
/// 1. `spawn { background: true }` **replies**, with a handle and `isError: false`;
/// 2. at that instant a real child process **has started** (the shim's marker) and **has not
///    finished** (the gate is still shut, and no contract exists);
/// 3. only then does the test open the gate;
/// 4. `wait` returns the child's contract.
///
/// **Why a synchronous implementation cannot pass.** Step 1's reply would be withheld until the
/// child exited; the child cannot exit until step 3; step 3 is downstream of step 1. The test
/// deadlocks and fails at [`DEADLOCK_BOUND`] naming the frame it was waiting on. That is the whole
/// negative control, and it depends on nothing being fast.
///
/// Step 2 is what makes it more than a shape assertion. A `spawn` that returned a handle and
/// started *nothing* would satisfy step 1 and step 4 could be made to satisfy itself; the started
/// marker is written by the child's own process, so it cannot be faked by marion's bookkeeping.
#[test]
fn a_backgrounded_spawn_returns_while_its_child_is_still_running() {
    let fx = fixture("background-ordering");
    let mut bridge = fx.bridge();

    let reply = bridge.tool("spawn", spawn_args(true));
    assert!(
        !is_error(&reply),
        "a backgrounded spawn that started is not an error: {reply}"
    );
    let text = text_of(&reply);
    assert!(
        text.contains("handle") && text.contains("not a result"),
        "the handle must say it is not an answer (§7.6's worked example), got: {text}"
    );
    assert!(
        text.contains("wait"),
        "and must name the verb that resolves it, got: {text}"
    );
    let task_id = handle_task_id(&reply);

    // The child really launched, and really has not finished. Both halves are read from the
    // *child's* own side of the world, not from marion's.
    let deadline = Instant::now() + DEADLOCK_BOUND;
    while fx.started_children() == 0 {
        assert!(
            Instant::now() < deadline,
            "the handle came back but no child process ever started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !fx.gate.exists(),
        "the gate is still shut, so the child cannot have exited — this is the ordering claim"
    );

    // Only now is the child allowed to finish.
    fx.open_gate();

    let collected = bridge.tool("wait", json!({"task_id": task_id}));
    let text = text_of(&collected);
    assert!(
        text.contains("\"task_id\"") && text.contains(&task_id),
        "wait returns the child's own contract: {text}"
    );
    assert!(
        text.contains("\"completion\""),
        "and a collected child has a completion, since it reached a terminal state: {text}"
    );

    assert!(bridge.close().success());
}

/// **`Spawned` is durable at the instant the process exists, and it names *that* process** —
/// design §11 item 28 step 1, and the record §6.1 step 7 asks for.
///
/// Until this, `run_spawn` journalled `Spawned` after the child had already been run **and
/// reaped**, so `pid` was `None` at every production writer and a pid captured there would have
/// named a process marion had watched die. Two live consequences, both closed by the assertion
/// below: `session/quit`'s `KillTree` preflight refused whenever there was anything to kill, and
/// item 30's two runaway shapes — *a live reparented process* and *a process already dead* — were
/// indistinguishable on disk.
///
/// **Three properties, and each rules out a different way of passing cheaply.**
///
/// 1. *A pid is recorded at all.* `pid: None` leaves `spawned_pids` reading `[None]`, which is
///    deliberately distinguishable from the record not having arrived.
/// 2. *It is the child's pid, not some live pid.* The shim names its own marker file `$$`, so the
///    journal is checked against a number the child wrote about itself. `std::process::id()` — the
///    bridge's own pid, the obvious wrong fill-in — is never in that set, and is named separately
///    so the failure says which mutation it caught.
/// 3. *The record is durable while the process is still running.* The gate is shut for the whole
///    of the assertion, so no child can have exited; moving the append back after `child.wait()`
///    means there is no record to read at all here, and the deadlock bound fails naming it.
///
/// Liveness is read three-valued through `ps` ([`marion_testsupport::liveness`]) and never as
/// `kill(pid, 0)`, which calls a zombie alive — the reading that would let property 3 pass over a
/// corpse the bridge had not yet reaped.
///
/// **The journal is read from a second process.** The bridge writes it; this test reads it. That
/// is not incidental: the whole value of the record is to a process that did not start the child,
/// and a same-process assertion could be satisfied by in-memory state that never reached disk.
#[test]
fn the_journal_names_the_childs_own_live_pid_while_the_child_is_still_running() {
    let fx = fixture("background-spawned-pid");
    let mut bridge = fx.bridge();

    let reply = bridge.tool("spawn", spawn_args(true));
    assert!(!is_error(&reply), "the spawn started: {reply}");
    let task_id = handle_task_id(&reply);

    // The child's own side of the world first: a process exists and has told us its pid.
    let deadline = Instant::now() + DEADLOCK_BOUND;
    while fx.started_children() == 0 {
        assert!(
            Instant::now() < deadline,
            "the handle came back but no child process ever started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let started = fx.started_pids();
    assert_eq!(
        started.len(),
        1,
        "exactly one child was asked for: {started:?}"
    );

    // Then marion's side, read off the file the bridge wrote.
    let mut pids = Vec::new();
    while pids.is_empty() {
        assert!(
            Instant::now() < deadline,
            "a child process is running and no `Spawned` record names it. Either the record is \
             still written after the child is reaped — which is what this test exists to forbid — \
             or it was never written at all."
        );
        pids = spawned_pids(&fx.journal_text());
        if pids.is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    assert_eq!(pids.len(), 1, "one child, one Spawned record: {pids:?}");

    // Property 3, asserted **before** the pid is looked at: the gate has not been opened, so the
    // process this record is about cannot have exited between the record and this line.
    assert!(
        !fx.gate.exists() && fx.finished_children() == 0,
        "the gate is shut and no child has exited, so `Spawned` is on disk about a process that \
         is still running — not a post-mortem"
    );

    // Property 1.
    let pid = pids[0].expect(
        "`Spawned` records no pid. An absence here is the state item 28 step 1 removes: a \
         non-terminal node marion cannot prove a signal reaches, and a runaway indistinguishable \
         from a process already dead.",
    );
    // Property 2.
    assert!(
        started.contains(&pid),
        "`Spawned` records pid {pid}, which is not a pid any child claimed for itself \
         ({started:?})"
    );
    assert_ne!(
        pid,
        bridge.pid(),
        "the recorded pid is the bridge's own, so the record names the process that did the \
         spawning rather than the one that was spawned"
    );
    assert_eq!(
        liveness(pid),
        Liveness::Alive,
        "the recorded pid must name a process that can still run code; a zombie or an absence \
         means the record was written about a child marion had already finished with"
    );

    // Only now is the child allowed to finish, and the run completes normally around the record.
    fx.open_gate();
    let collected = bridge.tool("wait", json!({"task_id": &task_id}));
    assert!(
        text_of(&collected).contains("\"completion\""),
        "the child still reaches a terminal state: {collected}"
    );
    assert_eq!(
        spawned_pids(&fx.journal_text()),
        vec![Some(pid)],
        "and the record is written once, at the spawn, not again at the reap"
    );
    assert!(bridge.close().success());
}

/// **A launch that fails before a process exists journals no `Spawned` record at all** — the other
/// half of item 28 step 1's claim, and the one that keeps `abandoned()` honest.
///
/// The pid assertion above says a record that *is* written names a live process. This says the
/// record is not written when there is nothing to name. Together they are what makes
/// `SpawnIntent`-and-nothing-after mean **no process exists, full stop** — §11 item 30's shapes 1
/// and 2 ceasing to be indistinguishable on disk — and what lets `handler.rs`'s `abandoned()` read
/// `SpawnIntent` + `SpawnAborted` as a node that strands nothing.
///
/// **The strictness is the safe direction and this is what pins it.** `abandoned()` requires no
/// `Spawned` and no pid; step 1 makes `Spawned` arrive far earlier, so a change that fired the
/// hook one line too early — before `command.spawn()` could fail — would leave a `Spawned` beside
/// the abort and hold the supervisor resident over a process that never existed. That is the
/// b600d82 case in reverse, and no argument distinguishes the two: only the record does.
///
/// `started_children() == 0` is the half that cannot be faked from marion's side. The shim writes
/// its own marker, so this is the child's account of never having run, not marion's.
#[test]
fn a_launch_that_fails_before_the_process_exists_journals_no_spawned_record() {
    let fx = fixture("background-launch-fails");
    let mut bridge = fx.bridge_with_an_unlaunchable_harness();

    let reply = bridge.tool("spawn", spawn_args(false));
    assert!(
        is_error(&reply),
        "a harness that cannot be executed is a refused spawn, not a silent one: {reply}"
    );

    assert_eq!(
        fx.started_children(),
        0,
        "no child process ever ran, by the children's own account"
    );
    assert_eq!(
        spawned_pids(&fx.journal_text()),
        Vec::new(),
        "and no `Spawned` record was written, so the intent alone means what it now claims to \
         mean: there is no process and there never was one"
    );
    let journal = fx.journal_text();
    assert!(
        journal.contains("SpawnIntent") && journal.contains("SpawnAborted"),
        "the intent is still resolved as an abort — §7.2: a node marion decided the fate of is \
         never one marion lost: {journal}"
    );
    assert!(bridge.close().success());
}

/// **A second `wait` on the same handle says so, rather than blocking forever.**
///
/// The other half of the handle's contract. A `JoinHandle` can be joined once; the mistake this
/// guards is answering the second `wait` by blocking on a consumed handle, which would hang the
/// caller's turn on a child that has already been delivered.
#[test]
fn a_collected_handle_cannot_be_collected_twice() {
    let fx = fixture("background-collect-twice");
    let mut bridge = fx.bridge();
    let task_id = handle_task_id(&bridge.tool("spawn", spawn_args(true)));
    fx.open_gate();
    // The shim never calls `report`, so this contract is `Unreported` and therefore `isError:
    // true` — which is `spawn_result`'s deliberate shape for a child that produced no answer, and
    // is *not* what this test is about. What matters is that the contract came back at all, so
    // the assertion is on the contract's presence rather than on the flag.
    let first = bridge.tool("wait", json!({"task_id": &task_id}));
    assert!(
        text_of(&first).contains("\"completion\""),
        "the first wait delivers the contract: {first}"
    );

    let again = bridge.tool("wait", json!({"task_id": &task_id}));
    assert!(is_error(&again), "a second collection is refused: {again}");
    let text = text_of(&again);
    assert!(
        text.contains("already") && text.contains(&task_id),
        "and says which handle and why, got: {text}"
    );
    assert!(bridge.close().success());
}

/// **`max_concurrent_children` refuses rather than queues** — §3.1, and inert until today.
///
/// `LIVE_CHILDREN_OF_A_SYNCHRONOUS_CALLER = 0` made this bound unreachable: 0 is never `>= 4`, so
/// the concurrency half of §6.1 step 2's gate was wired and could not fire. Backgrounding is what
/// gives a caller a second live child, and this is the first test in the repo that can observe the
/// bound at all.
///
/// **Refuses, not queues, and the difference is observable here rather than argued.** A queueing
/// implementation would answer the fifth `spawn` with a handle and start the child once a slot
/// freed; this asserts the fifth `spawn` comes back an **error, in the same frame**, while all four
/// predecessors are still blocked on the gate. It also asserts the refusal **names the bound**,
/// since §3.1 requires a refusal that says what it refused and a bare failure would not.
///
/// The count is verified from the children's own markers as well as from marion's answer: exactly
/// four processes exist, so the fifth was refused rather than started and discarded.
#[test]
fn the_fifth_concurrent_background_child_is_refused_in_the_same_frame() {
    let fx = fixture("background-concurrency");
    let mut bridge = fx.bridge();
    let max = marion_core::agent_type::builtin("claude")
        .expect("the caller's type resolves")
        .max_concurrent_children;

    for i in 0..max {
        let reply = bridge.tool("spawn", spawn_args(true));
        assert!(
            !is_error(&reply),
            "child {i} is within the bound and must be served: {reply}"
        );
    }

    let refused = bridge.tool("spawn", spawn_args(true));
    assert!(
        is_error(&refused),
        "the spawn past the bound is refused in the frame that asked for it, not handed a \
         handle to a child that will start later: {refused}"
    );
    let text = text_of(&refused);
    assert!(
        text.contains("max_concurrent_children"),
        "§3.1 wants a refusal that names the bound, got: {text}"
    );
    assert!(
        !text.contains("handle"),
        "and a refusal is never dressed as a handle, got: {text}"
    );

    // Every one of the four is still blocked, which is what makes the refusal a *concurrency*
    // refusal rather than an artefact of children finishing between calls.
    let deadline = Instant::now() + DEADLOCK_BOUND;
    while fx.started_children() < max as usize {
        assert!(
            Instant::now() < deadline,
            "expected {max} live children, saw {}",
            fx.started_children()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        fx.started_children(),
        max as usize,
        "the refused spawn started no process: a refusal creates nothing (§6.1 step 2 runs before \
         every side effect)"
    );

    fx.open_gate();
    assert!(bridge.close().success());
}

/// **`allow_concurrent_writes: false` is honoured structurally** — no two live children share a
/// cwd, because each one gets its own worktree.
///
/// §11 item 23 called this parameter inverted: with no §6.6 holder check in code, `true` was said
/// to be honoured accidentally and `false` to be the value marion could not honour. That reasoning
/// assumed `isolation` was live. It is not — `shared-cwd` is refused by name and `make_worktree`
/// runs unconditionally — so the guarantee `false` asks for is delivered by the code path rather
/// than by a check marion has to remember to run. This is that guarantee as an assertion instead of
/// a claim: with the maximum number of children alive **at once**, every workspace path is
/// distinct and none of them is the repo.
///
/// **It is also the git-serialization control, and it is a weak one — see `spawn::repo_write_guard`.**
/// Concurrent `git worktree add`/`remove` against one repository really does fail, but S17
/// (`tests/fixtures/s17/README.md`) measured the failure in `.git/worktrees/` bookkeeping rather
/// than in `index.lock`, and measured its rate: at four concurrent writers it needs a few hundred
/// iterations to appear. This test performs four `worktree add`s **once**, so it is three orders of
/// magnitude short of witnessing it, which is why deleting the guard leaves it green. Read it as
/// what it reliably is — **every** concurrent child got its own workspace and none was lost — and
/// not as evidence about the guard.
#[test]
fn concurrent_children_never_share_a_workspace_and_never_lose_one_to_a_git_lock() {
    let fx = fixture("background-worktrees");
    let mut bridge = fx.bridge();
    let max = marion_core::agent_type::builtin("claude")
        .expect("the caller's type resolves")
        .max_concurrent_children;

    let mut ids = Vec::new();
    for _ in 0..max {
        let reply = bridge.tool("spawn", spawn_args(true));
        assert!(
            !is_error(&reply),
            "every child within the bound starts: {reply}"
        );
        ids.push(handle_task_id(&reply));
    }
    fx.open_gate();

    let mut paths = Vec::new();
    for id in &ids {
        let reply = bridge.tool("wait", json!({"task_id": id}));
        let text = text_of(&reply);
        let contract: Value = serde_json::from_str(text.split_once('{').map_or(&text[..], |_| {
            &text[text.find('{').expect("a contract is json")..]
        }))
        .unwrap_or_else(|e| panic!("wait returned a contract for {id}: {e}: {text}"));
        let path = contract["workspace"]["Worktree"]["path"]
            .as_str()
            .unwrap_or_else(|| panic!("every child is isolated in a worktree: {contract}"))
            .to_string();
        assert_ne!(
            Path::new(&path),
            fx.repo,
            "a child's workspace is never the caller's own cwd"
        );
        paths.push(path);
    }

    assert_eq!(paths.len(), max as usize, "no child was lost to a git lock");
    let mut unique = paths.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        paths.len(),
        "no two concurrently live children share a workspace, which is what \
         `allow_concurrent_writes: false` asks for: {paths:?}"
    );

    assert!(bridge.close().success());
}

/// **The bridge does not leave at EOF while a child is still running** (§5.7).
///
/// The rule is *"a supervisor MUST NOT exit while any `spawn` is outstanding"*, and while `spawn`
/// blocked it was free — EOF was unreachable with a spawn in flight. Backgrounding makes it a real
/// obligation, and breaking it is worse than losing the child: the child's `SpawnIntent` is
/// journaled with no resolution, which §7.2 reads as a node marion **lost**, asserting an accident
/// about a process marion in fact chose to abandon.
///
/// Asserted as an ordering: stdin is closed while the gate is shut, the bridge is observed **still
/// alive**, the gate is opened, and only then does the bridge exit. A bridge that left at EOF would
/// be gone at the first check.
///
/// **What this does not claim, and it is the important half.** It is the EOF case only, and s16
/// measured that a real Claude Code harness **never produces one**: it sends SIGINT, SIGTERM 100 ms
/// later, then SIGKILL, all pid-targeted at the MCP server. So this hold does not run in
/// production, the backgrounded child outlives the SIGKILL as an untracked process reparented to
/// pid 1, and that is §11 item 30 — a recorded hole, not a solved problem. This test guards the
/// hold for the clients marion itself writes; it is not evidence that a child survives a harness
/// exit, and must not be read as any.
#[test]
fn the_bridge_waits_for_an_outstanding_child_before_leaving_at_eof() {
    let fx = fixture("background-eof-hold");
    let mut bridge = fx.bridge();
    assert!(!is_error(&bridge.tool("spawn", spawn_args(true))));

    let deadline = Instant::now() + DEADLOCK_BOUND;
    while fx.started_children() == 0 {
        assert!(Instant::now() < deadline, "no child process ever started");
        std::thread::sleep(Duration::from_millis(20));
    }

    // EOF, with the gate still shut and the child provably unfinished.
    assert_eq!(
        fx.finished_children(),
        0,
        "the child has not been released yet"
    );
    bridge.close_stdin();

    // **The ordering assertion.** The bridge's stdout reaches EOF exactly when the bridge exits,
    // so waiting on the reader is waiting on the bridge's own departure. Two outcomes and only one
    // of them is legal:
    //
    // * EOF arrives → the bridge left. Legal *only* if its child had already finished, which the
    //   child's own exit marker answers. With the gate shut it has not, so this is the §5.7
    //   violation, caught as an ordering between two observed events rather than as a duration.
    // * nothing arrives within the sampling window → the bridge is still here, holding. Open the
    //   gate and it leaves.
    //
    // The window is a *sample*, not a bound on correctness: a machine slow enough to miss an
    // early EOF simply takes the second branch and still passes, so this cannot fail falsely in
    // either direction. What it cannot miss is a bridge that leaves immediately, which is what
    // dropping the hold produces.
    match bridge.lines.recv_timeout(EOF_SAMPLE) {
        Ok(None) => panic!(
            "the bridge exited at EOF with {} child process(es) started and {} finished — §5.7 \
             forbids leaving while a spawn is outstanding, and the child's SpawnIntent is now \
             journaled with no resolution, which §7.2 reads as a node marion *lost*",
            fx.started_children(),
            fx.finished_children()
        ),
        Ok(Some(line)) => panic!("the bridge answered a frame nobody sent: {line}"),
        Err(RecvTimeoutError::Timeout) => {}
        Err(RecvTimeoutError::Disconnected) => panic!("the reader thread is gone"),
    }

    fx.open_gate();
    let status = bridge
        .child
        .wait()
        .expect("the bridge leaves once its child is done");
    assert!(status.success(), "and leaves cleanly: {status:?}");
    // The other half of the ordering, and a leak check in the same assertion: the bridge left
    // *after* its child, so the child's own exit marker must exist by now. Without this the
    // fixture could drop — deleting the gate file out from under a shim that had not yet noticed
    // it — and strand the child. Two were found stranded exactly that way on 2026-08-06, which is
    // the product's own hazard (§11 item 30) reproduced by accident in a test.
    assert_eq!(
        fx.finished_children(),
        1,
        "the bridge outlived its child, so the child has recorded its own exit"
    );
}

/// **A `timeout_secs` the machine's clock cannot represent is answered, not detonated** — B1, the
/// synchronous half.
///
/// `timeout_secs` is caller-controlled and typed `u64` all the way down: `main::handle_tool_call`
/// reads `args["timeout_secs"].as_u64()`, `run_spawn` turns it into a `Duration`, and
/// `run_bounded` computes `Instant::now() + timeout`. `Instant + Duration` **panics** on overflow,
/// and it does so *after* `command.spawn()` has already started a real OS process.
///
/// On this path the panic is on the bridge's **own** thread — `handle_tool_call` is called inline
/// from `run_bridge`'s read loop — so the unwind takes the whole MCP server down. Every other live
/// child of this node dies with it, over one arithmetic overflow a child agent can ask for by name.
///
/// The assertion is therefore the crudest possible one and deliberately so: **the bridge is still
/// there and it answered.** Before the fix `read_reply` sees the server's stdout close instead of a
/// reply and fails naming that, which is the failure mode, not a proxy for it.
///
/// The gate is opened before the spawn because this path is synchronous: the reply cannot arrive
/// until the child exits, and the child exits as soon as the gate is there.
#[test]
fn a_timeout_the_clock_cannot_represent_is_answered_rather_than_killing_the_bridge() {
    let fx = fixture("background-overflow-sync");
    fx.open_gate();
    let mut bridge = fx.bridge();

    let reply = bridge.tool("spawn", spawn_args_with_timeout(false, u64::MAX));
    let text = text_of(&reply);
    assert!(
        !text.contains("panicked"),
        "a caller-supplied wall clock is a number to bound, never a way to unwind marion: {text}"
    );
    assert!(
        text.contains("\"completion\""),
        "the child ran to a terminal state under a clock marion could hold: {text}"
    );

    // **The clamp is recorded, not silently substituted.** §6.7's rule everywhere else in
    // `run_spawn` is that the contract names the compiled value rather than the asked-for one, and
    // the wall clock obeys it: a reader of this contract can see exactly how long marion was
    // willing to hold the node, and can see that it is not what was requested.
    assert!(
        text.contains(&marion_supervisor::run::MAX_TIMEOUT_SECS.to_string()),
        "the contract records the bound marion actually enforced: {text}"
    );
    assert!(
        !text.contains(&u64::MAX.to_string()),
        "and never the unenforceable number that was asked for: {text}"
    );

    let journal = fx.journal_text();
    assert!(
        journal.contains("SpawnIntent"),
        "the node was journaled at all: {journal}"
    );
    assert!(
        !journal.contains("SpawnAborted"),
        "marion did not abandon the spawn path, so nothing may record that it did: {journal}"
    );
    assert!(bridge.close().success());
}

/// **The same number on the background path leaves no abandoned intent and no orphan** — B1.
///
/// Here the panic lands on the child's own thread, so the bridge survives and the damage is
/// quieter and worse. The sequence before the fix, in order:
///
/// 1. `run_spawn` journals `SpawnIntent`, makes the worktree and branch, writes the agent and
///    config directories, and **starts a real process** (the shim's marker proves it);
/// 2. `Instant::now() + Duration::from_secs(u64::MAX)` panics one line later;
/// 3. `AbortOnDrop` writes `SpawnAborted` on the way out — the record that says *marion decided
///    this node's fate*, which §7.2 reads as "no process ever existed";
/// 4. dropping `std::process::Child` does **not** kill anything, so the process it started is
///    still there, reparented and unenforced, with its worktree, branch and directories intact;
/// 5. `wait` converts the unwind into `SpawnError::Panicked`.
///
/// So the journal ends `SpawnIntent + SpawnAborted` — a *decided* node — while a live process and
/// its whole workspace remain. That mismatch is what this test refuses: the intent must be
/// resolved by the child actually running, and `SpawnAborted` must not appear at all.
#[test]
fn a_backgrounded_timeout_the_clock_cannot_represent_resolves_its_intent_rather_than_aborting_it() {
    let fx = fixture("background-overflow-bg");
    let mut bridge = fx.bridge();

    let reply = bridge.tool("spawn", spawn_args_with_timeout(true, u64::MAX));
    assert!(
        !is_error(&reply),
        "a backgrounded spawn that started is not an error: {reply}"
    );
    let task_id = handle_task_id(&reply);

    // A real process exists — measured from the child's own side, so it is not marion's bookkeeping
    // agreeing with itself.
    let deadline = Instant::now() + DEADLOCK_BOUND;
    while fx.started_children() == 0 {
        assert!(
            Instant::now() < deadline,
            "the handle came back but no child process ever started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    fx.open_gate();

    let collected = bridge.tool("wait", json!({"task_id": task_id}));
    let text = text_of(&collected);
    assert!(
        !text.contains("panicked"),
        "the child's thread must not die on a number the caller chose: {text}"
    );
    assert!(
        text.contains("\"completion\""),
        "the child reached a terminal state and its contract came back: {text}"
    );

    let journal = fx.journal_text();
    assert!(
        !journal.contains("SpawnAborted"),
        "a node whose process really ran must never be recorded as one marion walked away from — \
         that record is what makes the leaked process invisible: {journal}"
    );
    assert!(bridge.close().success());
}

/// **`max_concurrent_children` bounds how many children run at once, not how many this bridge may
/// ever start** — B4.
///
/// `Background::live_children` is the table's length and `wait` takes only the join handle, leaving
/// the entry behind; nothing but process exit ever pops one. So a caller that starts the maximum,
/// lets every one of them finish, and successfully collects every contract has **zero** running
/// processes, **zero** unjoined threads, and is still refused — for the life of the bridge. §3.1's
/// bound has silently become a lifetime quota.
///
/// The test is the direct statement of that: collect all of them, then ask for one more. It says
/// nothing about the *uncollected* case, which is a slot genuinely still occupied and is pinned
/// separately in `background.rs`'s own unit tests.
#[test]
fn a_collected_child_frees_the_concurrency_slot_it_was_holding() {
    let fx = fixture("background-slot-release");
    let mut bridge = fx.bridge();
    let max = marion_core::agent_type::builtin("claude")
        .expect("the caller's type resolves")
        .max_concurrent_children;

    let mut ids = Vec::new();
    for i in 0..max {
        let reply = bridge.tool("spawn", spawn_args(true));
        assert!(!is_error(&reply), "child {i} is within the bound: {reply}");
        ids.push(handle_task_id(&reply));
    }
    fx.open_gate();
    for id in &ids {
        let reply = bridge.tool("wait", json!({"task_id": id}));
        assert!(
            text_of(&reply).contains("\"completion\""),
            "every child is collected, so every slot it held is genuinely free: {reply}"
        );
    }

    let again = bridge.tool("spawn", spawn_args(true));
    assert!(
        !is_error(&again),
        "with every child terminal and every contract delivered there is nothing to be concurrent \
         with, so this spawn is within the bound: {again}"
    );
    assert!(
        !text_of(&again).contains("max_concurrent_children"),
        "and the refusal that must not happen is the concurrency one: {}",
        text_of(&again)
    );
    assert!(bridge.close().success());
}

/// **A harness that hangs answering `--version` does not hang the bridge** — B3's root cause.
///
/// The child's `timeout_secs` bounds exactly one thing: the harness invocation, inside
/// `run_bounded`. `run_spawn` then asks the harness what version it is — *after* the child has been
/// reaped — and that probe used a plain `Command::output`, which blocks until the process closes
/// its pipes and has no deadline at all. So a harness that ran fine and then hung on `--version`
/// left `run_spawn` unable to return, and on the background path left `wait` blocked on a thread
/// that would never finish.
///
/// **And a blocked `wait` is not one stuck caller.** `main::run_bridge` reads and dispatches frames
/// on one thread, calling `handle_tool_call` inline, so nothing after it is even read: no sibling's
/// `spawn`, no other `wait`, no `report`. The last two assertions are that half — the bridge is
/// still serving afterwards, and still leaves cleanly.
///
/// The absent version is recorded as *absent*, never guessed: a node's whole contract must not be
/// lost because a version string did not arrive, and a fabricated one would be worse than either.
#[test]
fn a_harness_that_hangs_answering_its_version_does_not_hang_the_bridge() {
    let fx = fixture("background-slow-version");
    fx.make_version_slow();
    let mut bridge = fx.bridge();

    let task_id = handle_task_id(&bridge.tool("spawn", spawn_args(true)));
    fx.open_gate();

    let collected = bridge.tool("wait", json!({"task_id": &task_id}));
    let text = text_of(&collected);
    assert!(
        text.contains("\"completion\""),
        "the child ran and reached a terminal state, so its contract comes back whatever the \
         version probe did: {text}"
    );
    assert!(
        text.contains("\"version\": \"unknown\""),
        "a probe that expired is recorded as an absence, never guessed: {text}"
    );

    // The bridge is still reading frames — which is the half of this that a `wait`-only assertion
    // would miss, since an unbounded probe stalls the read loop rather than just this caller.
    let after = bridge.tool("wait", json!({"task_id": "task-nobody-started"}));
    assert!(
        is_error(&after) && !text_of(&after).is_empty(),
        "the bridge still answers frames after a child overran its version probe: {after}"
    );
    assert!(bridge.close().success());
}
