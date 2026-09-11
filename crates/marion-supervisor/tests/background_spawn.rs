//! **The bridge is a courier** — §11 item 28 step 5 — and `spawn { background: true }` really is
//! not synchronous.
//!
//! Driven through the **real `marion-supervisor mcp` binary over stdio**, the way a harness drives
//! it, against a **real detached supervisor** started the way `detach.rs` stage 3 starts one. Both
//! halves are real processes because the property under test is a property of processes: which one
//! owns a child, and what happens to that child when the other one dies.
//!
//! # The bed, and why each piece of it is a real thing
//!
//! Every test here needs a caller with a name and a capability, because since step 5 that is what a
//! `spawn` presents: `SpawnCaller { agent_id, node_token }`, checked against the supervisor's own
//! table. There is exactly one honest way to obtain those — be a node the supervisor started — and
//! so the fixture does the whole bootstrap:
//!
//! 1. a supervisor, spawned as stage 3 with the shim ahead of `codex` on its `PATH` (the supervisor
//!    is what `exec`s a harness now, so its `PATH` is the one that decides which binary runs);
//! 2. a **root**, created over the socket with `caller: None` — the shim again, blocked on a gate
//!    the fixture holds shut for the whole test, so the caller stays non-terminal;
//! 3. the bridge, started with **the environment marion itself wrote into that root's declaration**,
//!    read back off the `config.toml` the supervisor generated. Not a hand-written env block: a
//!    declaration marion wrote is the only thing a real harness ever hands a bridge, and reading it
//!    back is what makes these tests fail if `bridge_env_pairs` ever stops carrying the token.
//!
//! # The child is a shim, and that is the point
//!
//! The shim is a real process that marion really launches, really waits on and really reaps — what
//! it is not is a language model. It **blocks until the test creates a gate file**, and that is the
//! entire reason this file can make an ordering claim at all:
//!
//! > if `spawn` were synchronous, the reply to the `spawn` frame could not arrive until the child
//! > exited, and the child cannot exit until the test — which is blocked reading that reply —
//! > creates the gate file. A synchronous implementation **deadlocks**.
//!
//! No elapsed time is measured anywhere and no assertion says "this was fast". The bound in
//! [`Bridge::read_reply`] exists solely so a deadlock is a loud failure naming the deadlock instead
//! of a suite that hangs, which is `duplex.rs`'s self-destruct idiom applied to a test.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test background_spawn
//! ```
//!
//! It needs **no harness binary, no network and no credential**: the shim replaces `codex` for the
//! root and for every child, and nothing here reaches a model.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_core::proto::params::AgentSpawnParams;
use marion_core::proto::{Call, Method, MethodResult};
use marion_supervisor::socket::project_root;
use marion_testsupport::{Liveness, Scratch, fixture_repo, liveness, scratch, survivors, sweep};
use serde_json::{Value, json};

mod common;
use common::{Supervisor, declaration_of, walk};

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// A bound that exists **only to fail**, never to be reached on a passing run.
///
/// Every reply this file waits for is produced by a bridge and a supervisor that are not waiting on
/// anything the test has not already done, so the passing path takes milliseconds. Reaching this
/// bound means something is genuinely blocked — which, for the ordering control, is precisely the
/// bug under test. It is therefore deliberately generous: a bound tight enough to be a *timing*
/// assertion would turn a slow machine into a false failure.
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

/// How long a departure control samples for an early death.
///
/// **Not a correctness bound.** A child that died with its bridge dies within milliseconds of it,
/// so this only has to be longer than a process teardown; a machine slow enough to exceed it takes
/// the test's other branch and still passes. It is deliberately *not* named a timeout: nothing
/// fails because this elapsed.
const DEPARTURE_SAMPLE: Duration = Duration::from_secs(3);

/// §5.7's idle grace for the fixture's supervisor.
///
/// Generous on purpose and never waited out: every test holds a live root, so §5.7's exclusion list
/// keeps the supervisor resident regardless, and the fixture ends it explicitly. A short grace would
/// make a test that opened its gates early race a supervisor that had decided to leave.
const IDLE_GRACE: Duration = Duration::from_secs(600);

/// Appears in the **root's** argv and nowhere else, so the shim can tell the one invocation that
/// must outlive the test from the children that must not.
const ROOT_MARKER: &str = "MARION-BACKGROUND-SPAWN-ROOT-a41f";

/// A `codex` that blocks until the test says otherwise.
///
/// * `--version` answers immediately — unless the `slow_version` marker exists, which is how one
///   test reproduces a harness that hangs on the version probe.
/// * An invocation whose argv carries [`ROOT_MARKER`] is **the root**: it waits on the root gate,
///   which the fixture opens only as it tears down, and writes no child markers.
/// * Anything else is a child: it writes a *started* marker — which is what lets a test assert a
///   real process existed, rather than inferring it — then polls for the gate file and exits 0,
///   writing a *done* marker on the way out.
///
/// It writes nothing to stdout, so a child's contract lands `Unreported`. That is correct and
/// irrelevant: this file asserts about *when* contracts arrive and *whether* spawns are refused,
/// never about what a child said.
fn shim(
    dir: &Path,
    gate: &Path,
    root_gate: &Path,
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
    # (§6.1 step 3), is a claim about the *pre-launch* path. It sleeps rather than waiting for a
    # gate, so the hang does not depend on either gate's state; the sleep is the shim's own life cap,
    # so no bad run strands it indefinitely.
    if [ -e {slow_version} ]; then sleep {life_secs}; fi
    echo "codex-cli 0.146.0-marion-shim"; exit 0 ;;
esac
# The root is told apart by its own argv and waits on its own gate: it must outlive every child,
# because it is the caller whose `agent_id` and capability token every `spawn` in this file
# presents. It writes no marker, so a test counting children counts children.
case "$*" in
  *{root_marker}*) wait_for={root_gate} ;;
  *) wait_for={gate}
     mkdir -p {started} {done}
     # A distinct marker per invocation, so a test can count the children that really launched.
     : > {started}/$$ ;;
esac
# **Mortal by construction.** The gate lives in a `Scratch` directory that is removed when the
# test's fixture drops, so a shim that only ever waited for the gate would spin forever if the test
# panicked. This is S16's own lesson applied to a test fixture: a probe that cannot die on its own
# is a leak waiting for a bad run, and the cap must be shorter than the child's contract timeout so
# it is never what ends a passing run.
waited=0
while [ ! -e "$wait_for" ]; do
  sleep 0.05
  waited=$((waited + 1))
  if [ "$waited" -gt {life_ticks} ]; then
    exit 0
  fi
done
case "$*" in
  *{root_marker}*) : ;;
  # A second marker at exit, so "the child has finished" is an event the test can observe rather
  # than a time it has to guess.
  *) : > {done}/$$ ;;
esac
exit 0
"#,
        started = shell_quote(started_dir),
        done = shell_quote(done_dir),
        gate = shell_quote(gate),
        root_gate = shell_quote(root_gate),
        root_marker = ROOT_MARKER,
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

// ---------------------------------------------------------------------------------------------
// The supervisor, and the root whose declaration every bridge here is started from.
// ---------------------------------------------------------------------------------------------

/// One bridge process, driven the way a harness drives it.
struct Bridge {
    child: Child,
    /// `Option` so stdin can be closed *without* consuming the `Bridge` — the departure tests have
    /// to close it and then keep observing, and `Drop`'s cleanup must survive a panic in between.
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
    /// Start `marion-supervisor mcp` with **the declaration marion wrote for the fixture's root**,
    /// optionally with one key overridden.
    ///
    /// The environment is set on the child process, never on the test process: it is global state
    /// and a test that mutated its own would leak into every other test in this binary.
    fn start(declaration: &BTreeMap<String, String>, overrides: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_marion-supervisor"));
        cmd.arg("mcp");
        for (k, v) in declaration {
            cmd.env(k, v);
        }
        for (k, v) in overrides {
            cmd.env(k, v);
        }
        let mut child = cmd
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
    /// a real Claude Code harness does **not** do this — see the departure tests.
    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    /// SIGKILL, waited on. **Uncatchable, and that is the point**: it is what s16 measured a real
    /// harness doing to its MCP server, so nothing the bridge might run on the way out can be what
    /// keeps a child alive.
    fn kill_hard(&mut self) {
        // SAFETY: `kill` on the pid of a child this process spawned and has not reaped.
        unsafe { kill(self.child.id() as i32, 9) };
        let _ = self.child.wait();
        self.stdin = None;
    }

    /// Close stdin and wait for the bridge to leave.
    fn close(mut self) -> std::process::ExitStatus {
        self.close_stdin();
        self.child.wait().expect("the bridge exits on EOF")
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // A test that panicked mid-conversation must not leave a bridge behind. `kill` is
        // unconditional and its error ignored: the process may already be gone, which is the
        // outcome this wants.
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
    gate: PathBuf,
    root_gate: PathBuf,
    started: PathBuf,
    done: PathBuf,
    slow_version: PathBuf,
    shim_dir: PathBuf,
    supervisor: Supervisor,
    /// The node every bridge in this file serves, and whose capability token it presents.
    root_id: AgentId,
    /// The `env` block marion wrote into that root's own declaration.
    declaration: BTreeMap<String, String>,
    needle: String,
}

/// The caller's agent type, which is the root's — §6.1 step 2's gates read the **caller's** bounds.
const CALLER_TYPE: &str = "codex";

fn fixture(tag: &str) -> Fixture {
    let s = scratch(tag);
    let dir = s.to_path_buf();
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    let shim_dir = dir.join("bin");
    let started = dir.join("started");
    let done = dir.join("done");
    let gate = dir.join("gate");
    let root_gate = dir.join("root-gate");
    let slow_version = dir.join("slow-version");
    std::fs::create_dir_all(&shim_dir).expect("the shim dir");
    std::fs::create_dir_all(&state).expect("the state dir");
    shim(&shim_dir, &gate, &root_gate, &started, &done, &slow_version);

    // **The whole search path is stated, not extended.** `execvp` does not stop at the first match:
    // a `PATH` entry whose `codex` fails to exec is skipped and the search continues, so a test that
    // deliberately breaks the shim would otherwise launch the machine's real harness — which is how
    // that was discovered rather than reasoned about. The two system directories are *asserted* to
    // hold no `codex` rather than assumed to, so the bed states its own precondition; `git`, `sh`
    // and `ps` — the only other programs the spawn path shells out to — live in them.
    for dir in ["/usr/bin", "/bin"] {
        assert!(
            !Path::new(dir).join("codex").exists(),
            "{dir} holds a `codex`, so this bed could launch a real harness and assert nothing"
        );
    }
    let path_env = format!("{}:/usr/bin:/bin", shim_dir.to_string_lossy());
    let key = project_root(&repo);
    let supervisor = Supervisor::start(
        &state,
        &key,
        &path_env,
        // Nothing here reaches it: every harness invocation is the shim.
        "http://127.0.0.1:8099/v1",
        IDLE_GRACE,
    );

    // The root, over the socket, with `caller: None` — the one spawn in this file that needs no
    // capability, because there is no node behind it yet (`handler::root_spawn_authorized`).
    let answered = supervisor
        .call(Call::AgentSpawn(AgentSpawnParams {
            agent_type: CALLER_TYPE.into(),
            prompt: format!("{ROOT_MARKER}: hold until this fixture is torn down"),
            native_launch: None,
            caller: None,
            repo: Some(repo.clone()),
            acceptance_criteria: vec![],
            writable_scope: vec![],
            timeout_secs: Some(SHIM_LIFE.as_secs()),
            model: None,
            // §9's change record declined, deliberately: it is a `git add -A` walk of the operator's
            // own checkout on every fixture in this file, and nothing here asserts on it.
            no_change_record: Some(true),
            pane: None,
            // A root: `isolation` and `allow_concurrent_writes` are child-only and refused
            // beside `caller: None` (§6.6, §9).
            isolation: None,
            allow_concurrent_writes: None,
        }))
        .expect("the root is created over the socket");
    let MethodResult::AgentSpawn(root) = Method::AgentSpawn
        .decode_result(&answered)
        .expect("a readable agent/spawn result")
    else {
        panic!("agent/spawn answers with an agent/spawn result");
    };
    assert_eq!(
        root.task_id, None,
        "§9: a root has no task contract, so nothing may name one for it"
    );
    let declaration = declaration_of(&state, &root.agent_id);
    Fixture {
        needle: dir.to_string_lossy().to_string(),
        _scratch: s,
        repo,
        state,
        gate,
        root_gate,
        started,
        done,
        slow_version,
        shim_dir,
        supervisor,
        root_id: root.agent_id,
        declaration,
    }
}

impl Drop for Fixture {
    /// **Release every shim, then end the supervisor, then prove nothing is left.**
    ///
    /// In that order and not the other: the supervisor does not signal a node's process group when
    /// it is killed, so a fixture that killed it first would leave every blocked shim waiting out
    /// its own life cap. The gates are opened first so the ordinary path ends the children the way
    /// a run ends them, and `sweep` is the safety net for the run where it did not.
    fn drop(&mut self) {
        let _ = std::fs::write(&self.gate, b"go");
        let _ = std::fs::write(&self.root_gate, b"go");
        let deadline = Instant::now() + DEPARTURE_SAMPLE;
        while Instant::now() < deadline && !survivors(&self.needle).is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.supervisor.stop();
        let left = sweep(&self.needle);
        assert!(
            left.is_empty(),
            "this fixture left live processes behind: {left:?}"
        );
    }
}

impl Fixture {
    /// A bridge started the way the node's harness starts one: the declaration marion wrote, over
    /// the `PATH` that harness was launched with.
    ///
    /// **The `PATH` decides nothing here, and carrying it is the point.** A bridge starts no
    /// process since §11 item 28 step 5, so the shim being reachable from it changes no passing
    /// run. It is carried because a real bridge inherits its harness's environment, and because a
    /// mutation that restores an in-process `run_spawn` must then fail on *what it broke* — the
    /// node outliving the bridge — rather than on not finding a harness binary. A test whose kill
    /// depends on an accident of `PATH` is not measuring what it claims to.
    fn bridge(&self) -> Bridge {
        Bridge::start(
            &self.declaration,
            &[(
                "PATH",
                &format!(
                    "{}:{}",
                    self.shim_dir.to_string_lossy(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )],
        )
    }

    /// A bridge whose declaration carries a token the supervisor never minted.
    fn bridge_with_a_forged_token(&self) -> Bridge {
        Bridge::start(&self.declaration, &[("MARION_NODE_TOKEN", "not-the-token")])
    }

    /// A bridge pointed at a project whose supervisor does not exist.
    ///
    /// The socket path is *derived* from `MARION_STATE_DIR` and the tree, so this is the only way to
    /// aim a bridge somewhere else — there is deliberately no environment variable that names the
    /// socket, and that absence is what the test using this depends on.
    fn bridge_with_no_supervisor(&self, empty_state: &Path) -> Bridge {
        Bridge::start(
            &self.declaration,
            &[("MARION_STATE_DIR", &empty_state.to_string_lossy())],
        )
    }

    /// Break the shim so that a launch fails **inside `command.spawn()`**: the interpreter does not
    /// exist, so the file is still found and still executable and the failure is the exec.
    fn break_the_harness(&self) {
        let bin = self.shim_dir.join("codex");
        std::fs::write(&bin, "#!/marion/no/such/interpreter\nexit 0\n")
            .expect("the broken shim is written");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
            .expect("the broken shim is executable");
    }

    /// Make the shim hang when asked its version, from the next invocation on.
    fn make_version_slow(&self) {
        std::fs::write(&self.slow_version, b"hang").expect("the slow-version marker is written");
    }

    /// Release every blocked child. The root has its own gate and is untouched.
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
    /// *which* process it started, rather than only about how many.
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

    /// How many shim children have actually *exited*. The other end of the same measurement.
    fn finished_children(&self) -> usize {
        std::fs::read_dir(&self.done)
            .map(|d| d.flatten().count())
            .unwrap_or(0)
    }

    /// Wait until at least `n` children have started, or fail naming what was seen.
    fn await_children(&self, n: usize) {
        let deadline = Instant::now() + DEADLOCK_BOUND;
        while self.started_children() < n {
            assert!(
                Instant::now() < deadline,
                "expected {n} child process(es) to have started, saw {}",
                self.started_children()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Every `journal.jsonl` under this fixture's state dir, concatenated.
    ///
    /// The project hash that names the directory is derived inside marion, so the path is
    /// discovered rather than reconstructed — a test that recomputed the hash would be asserting
    /// against its own copy of the derivation instead of against the file marion wrote.
    fn journal_text(&self) -> String {
        let mut out = String::new();
        walk(&self.state, &mut |p| {
            if p.file_name().is_some_and(|n| n == "journal.jsonl") {
                out.push_str(&std::fs::read_to_string(p).unwrap_or_default());
            }
        });
        out
    }

    /// The `pid` of every `Spawned` record about a node that is **not the root**, in order.
    ///
    /// `Option<i32>` and not `i32`, because the two absences this file has to keep apart are *"no
    /// `Spawned` record has been written yet"* (an empty vector) and *"a `Spawned` record was
    /// written and records no pid"* (a `None` element). Collapsing them would let the record's own
    /// regression — going back to `pid: None` — read as a record that had not arrived yet, which is
    /// the one mutation the pid assertion exists to catch.
    ///
    /// The root is excluded because the fixture's own caller is a node too, and its pid is not the
    /// news any test here is about.
    fn child_spawned_pids(&self) -> Vec<Option<i32>> {
        self.journal_text()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| v.get("kind").and_then(|k| k.get("Spawned")).cloned())
            .filter(|s| s.get("agent_id").and_then(Value::as_str) != Some(&self.root_id.0))
            .map(|s| s.get("pid").and_then(Value::as_i64).map(|p| p as i32))
            .collect()
    }

    /// The one child agent directory this fixture's node has, discovered rather than computed.
    fn child_dir(&self) -> PathBuf {
        let mut dirs = Vec::new();
        let mut stack = vec![self.state.clone()];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if !p.is_dir() {
                    continue;
                }
                if d.file_name().is_some_and(|n| n == "agents") {
                    if !p.to_string_lossy().contains(&self.root_id.0) {
                        dirs.push(p);
                    }
                } else {
                    stack.push(p);
                }
            }
        }
        assert_eq!(
            dirs.len(),
            1,
            "exactly one child was asked for, and its stream is what this asserts about: {dirs:?}"
        );
        dirs.pop().expect("checked")
    }
}

/// How many bytes a node's stream holds right now. `0` for a stream that does not exist yet, which
/// is distinguishable here only because every use below compares two readings of the same path.
fn stream_len(agent_dir: &Path) -> u64 {
    std::fs::metadata(agent_dir.join("events.jsonl"))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// Wait for a node's stream to grow past `was`, or fail saying it did not.
fn await_growth(agent_dir: &Path, was: u64, why: &str) {
    let deadline = Instant::now() + DEADLOCK_BOUND;
    while stream_len(agent_dir) <= was {
        assert!(
            Instant::now() < deadline,
            "{why}: {} is still {was} bytes",
            agent_dir.join("events.jsonl").display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------------------------

/// **THE ordering control: the caller regains the turn while its child is still running.**
///
/// Stated as an ordering, never as a duration. The sequence, in the order the test forces it:
///
/// 1. `spawn { background: true }` **replies**, with a handle and `isError: false`;
/// 2. at that instant a real child process **has started** (the shim's marker) and **has not
///    finished** (the gate is still shut);
/// 3. only then does the test open the gate;
/// 4. `wait` returns the child's contract.
///
/// **Why a synchronous implementation cannot pass.** Step 1's reply would be withheld until the
/// child exited; the child cannot exit until step 3; step 3 is downstream of step 1. The test
/// deadlocks and fails at [`DEADLOCK_BOUND`] naming the frame it was waiting on.
///
/// Step 2 is what makes it more than a shape assertion. A `spawn` that returned a handle and
/// started *nothing* would satisfy step 1, and the started marker is written by the child's own
/// process, so it cannot be faked by marion's bookkeeping.
#[test]
fn a_backgrounded_spawn_returns_while_its_child_is_still_running() {
    let fx = fixture("bg-ordering");
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

    fx.await_children(1);
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
        "wait returns the child's own contract, read back from the file the supervisor wrote under \
         the id `agent/spawn` named: {text}"
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
/// **Three properties, and each rules out a different way of passing cheaply.**
///
/// 1. *A pid is recorded at all.* `pid: None` leaves the reading `[None]`, which is deliberately
///    distinguishable from the record not having arrived.
/// 2. *It is the child's pid, and nobody else's.* The shim names its own marker file `$$`, so the
///    journal is checked against a number the child wrote about itself — and the two plausible
///    wrong fill-ins, the bridge's pid and the supervisor's, are ruled out by name. The second is
///    new since step 5: the process doing the spawning is the supervisor now, so
///    `std::process::id()` inside `run_spawn` would be *its* pid rather than the bridge's.
/// 3. *The record is durable while the process is still running.* The gate is shut for the whole of
///    the assertion, so no child can have exited; moving the append back after `child.wait()` means
///    there is no record to read at all here.
///
/// Liveness is read three-valued through `ps` ([`marion_testsupport::liveness`]) and never as
/// `kill(pid, 0)`, which calls a zombie alive.
///
/// **The journal is read from a third process.** The supervisor writes it, the bridge asked for it,
/// and this test reads it — which is the whole value of the record.
#[test]
fn the_journal_names_the_childs_own_live_pid_while_the_child_is_still_running() {
    let fx = fixture("bg-spawned-pid");
    let mut bridge = fx.bridge();

    let reply = bridge.tool("spawn", spawn_args(true));
    assert!(!is_error(&reply), "the spawn started: {reply}");
    let task_id = handle_task_id(&reply);

    fx.await_children(1);
    let started = fx.started_pids();
    assert_eq!(
        started.len(),
        1,
        "exactly one child was asked for: {started:?}"
    );

    let deadline = Instant::now() + DEADLOCK_BOUND;
    let mut pids = Vec::new();
    while pids.is_empty() {
        assert!(
            Instant::now() < deadline,
            "a child process is running and no `Spawned` record names it. Either the record is \
             still written after the child is reaped — which is what this test exists to forbid — \
             or it was never written at all."
        );
        pids = fx.child_spawned_pids();
        if pids.is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    assert_eq!(pids.len(), 1, "one child, one Spawned record: {pids:?}");

    // Property 3, asserted **before** the pid is looked at.
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
        "the recorded pid is the bridge's own, so the record names a courier rather than the node"
    );
    assert_ne!(
        pid,
        fx.supervisor.child.id() as i32,
        "the recorded pid is the supervisor's own — the process that did the spawning — rather \
         than the process that was spawned"
    );
    assert_eq!(
        liveness(pid),
        Liveness::Alive,
        "the recorded pid must name a process that can still run code; a zombie or an absence \
         means the record was written about a child marion had already finished with"
    );

    fx.open_gate();
    let collected = bridge.tool("wait", json!({"task_id": &task_id}));
    assert!(
        text_of(&collected).contains("\"completion\""),
        "the child still reaches a terminal state: {collected}"
    );
    assert_eq!(
        fx.child_spawned_pids(),
        vec![Some(pid)],
        "and the record is written once, at the spawn, not again at the reap"
    );
    assert!(bridge.close().success());
}

/// **The bridge's death does not touch the node** — T4, and the whole of what §11 item 28 step 5
/// buys.
///
/// This test could not be written before step 5, because before step 5 it was false: the child was
/// a process the *bridge* had spawned, running on a thread the bridge owned, and s16 measured what
/// a real Claude Code harness does to its MCP server — SIGINT, SIGTERM 100 ms later, SIGKILL
/// ~450 ms after that. The child outliving that was §11 item 30's runaway: reparented to pid 1,
/// wall clock unenforced because the enforcer was in the bridge, `SpawnIntent` unresolved.
///
/// **SIGKILL, because it is uncatchable.** Nothing the bridge might run on its way out can be what
/// keeps the node alive; the node is alive because it was never the bridge's.
///
/// **And the assertion is liveness, not presence.** A pid in the process table can be a zombie and
/// a live process can have stopped working, so the load-bearing clause is that the node's own
/// `events.jsonl` **grew after the kill** — work marion recorded for a node whose bridge no longer
/// exists. The growth is *caused* after the kill, not merely observed after it: the gate is opened
/// only once the bridge is confirmed gone, and the child cannot exit until it is.
#[test]
fn a_bridge_killed_mid_child_leaves_the_node_running_and_its_stream_growing() {
    let fx = fixture("bg-kill-bridge");
    let mut bridge = fx.bridge();
    assert!(!is_error(&bridge.tool("spawn", spawn_args(true))));
    fx.await_children(1);

    let child_pid = fx.started_pids()[0];
    let child_dir = fx.child_dir();
    let before = stream_len(&child_dir);
    assert_eq!(
        fx.finished_children(),
        0,
        "the child has not been released yet, so everything after the kill is caused after it"
    );

    let bridge_pid = bridge.pid();
    bridge.kill_hard();
    assert_eq!(
        liveness(bridge_pid),
        Liveness::Gone,
        "the bridge must really be gone before anything below is attributed to its absence"
    );
    assert_eq!(
        liveness(child_pid),
        Liveness::Alive,
        "the child was the supervisor's, not the bridge's: killing a courier must not end a node"
    );

    // Now let the child finish. Everything the node writes from here is written for a node whose
    // bridge no longer exists.
    fx.open_gate();
    await_growth(
        &child_dir,
        before,
        "the node's stream stopped growing when its bridge was killed, so the child was the \
         bridge's after all",
    );
    let deadline = Instant::now() + DEADLOCK_BOUND;
    while fx.finished_children() == 0 {
        assert!(
            Instant::now() < deadline,
            "the child never reached its own exit after the bridge was killed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // **Waited for, because it is a later record than the one above.** `finished_children` counts
    // markers the child shim writes as *it* exits; `ContractPersisted` is marion's, written after
    // the reap, the contract build, the file and the closing bookend. Reading the journal the
    // instant the child's marker appeared asserted a record the supervisor had not reached yet, and
    // failed roughly one run in ten. Nothing is being raced past here — the record genuinely
    // arrives later, and there is no ordering rule this could assert instead; the defect was
    // waiting for one thing and asserting another.
    // Its own, much shorter budget. The record follows the child's exit by a reap and three
    // writes — milliseconds — so `DEADLOCK_BOUND` here would turn a record that is never coming
    // into a minute of waiting before saying so. Fifteen seconds is still two orders of magnitude
    // of headroom, and the failure prints the journal rather than merely reporting elapsed time.
    let contract_deadline = Instant::now() + Duration::from_secs(15);
    while !fx.journal_text().contains("ContractPersisted") {
        assert!(
            Instant::now() < contract_deadline,
            "the node ran to a contract with nobody holding its handle — the supervisor owns the \
             lifecycle, so the answer being undeliverable does not make the work stop: {}",
            fx.journal_text()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// **A bridge that leaves at EOF leaves its children running** — the same property through the
/// orderly departure, which is the one a bridge can still act on.
///
/// This is the inversion of `the_bridge_waits_for_an_outstanding_child_before_leaving_at_eof`, and
/// the inversion is deliberate rather than a regression. That test pinned `Background::join_all`: a
/// bridge that owned a child had to hold the process open at EOF, because leaving would kill the
/// child mid-run and leave its `SpawnIntent` unresolved — §5.7's *"MUST NOT exit while any `spawn`
/// is outstanding"*, read one level down. Nothing is outstanding here any more. The child is a node
/// of the supervisor's, so §5.7 binds the process that owns the lifecycle, and this one is free to
/// go the instant its stdin closes.
///
/// SIGKILL is covered separately; this one exists because it is the departure where the bridge
/// *does* get to run code, and therefore the one where a well-meaning "clean up my children on the
/// way out" could be written.
#[test]
fn a_bridge_that_leaves_at_eof_leaves_its_children_running() {
    let fx = fixture("bg-eof");
    let mut bridge = fx.bridge();
    assert!(!is_error(&bridge.tool("spawn", spawn_args(true))));
    fx.await_children(1);

    let child_pid = fx.started_pids()[0];
    let child_dir = fx.child_dir();
    let before = stream_len(&child_dir);
    assert_eq!(fx.finished_children(), 0, "the child is still blocked");

    assert!(
        bridge.close().success(),
        "the bridge leaves cleanly at EOF, with nothing to wait for"
    );

    // The sample is not a correctness bound: a child killed with its bridge dies within
    // milliseconds, and a machine slow enough to miss that still passes on the liveness reading.
    std::thread::sleep(DEPARTURE_SAMPLE);
    assert_eq!(
        liveness(child_pid),
        Liveness::Alive,
        "the child died with the bridge that asked for it — §5.7 now binds the supervisor, which \
         is still running, and a bridge must not take a node with it"
    );
    assert_eq!(
        fx.finished_children(),
        0,
        "and it did not finish early either: the gate is still shut"
    );

    fx.open_gate();
    await_growth(
        &child_dir,
        before,
        "the node's stream stopped growing when its bridge left",
    );
}

/// **A spawn whose supervisor is not listening is refused, and starts nothing.**
///
/// The dial is the only path: there is deliberately no in-process fallback, because a fallback is
/// invisible — the same tool result, a real child, and a live node no supervisor owns, can kill, or
/// can hand to a re-attaching client. `bin/marion.rs` made the same choice for the client at step 6.
///
/// The bridge is aimed at an empty state directory, which is the *only* way to aim one somewhere
/// else: §2's socket path is derived from `<state>` and the tree, and no environment variable names
/// it. So this test also pins that absence.
///
/// `started_children() == 0` is the half that cannot be faked from marion's side — the children's
/// own account of never having run.
#[test]
fn a_spawn_whose_supervisor_is_not_listening_is_refused_and_starts_nothing() {
    let fx = fixture("bg-no-supervisor");
    let empty = fx.state.parent().expect("scratch root").join("empty-state");
    std::fs::create_dir_all(&empty).expect("an empty state dir");
    let mut bridge = fx.bridge_with_no_supervisor(&empty);

    let reply = bridge.tool("spawn", spawn_args(false));
    assert!(
        is_error(&reply),
        "an unreachable supervisor is a refusal, not a quietly in-process spawn: {reply}"
    );
    let text = text_of(&reply);
    assert!(
        text.contains("supervisor"),
        "the refusal is in the bridge's own voice and names what was not there: {text}"
    );
    assert!(
        text.contains(".sock"),
        "and names the socket it derived, since that is the whole diagnosis: {text}"
    );
    assert!(
        text.contains("Nothing was started"),
        "and says nothing happened — the claim a fallback would make false: {text}"
    );
    assert_eq!(
        fx.started_children(),
        0,
        "no child process ever ran, by the children's own account"
    );
    assert!(bridge.close().success());
}

/// **A spawn presenting a token this supervisor did not mint is refused, and journals nothing.**
///
/// §5.4's capability, at the point where it is spent. The bridge's identity is no longer a claim
/// only the bridge can see: it travels on a socket any process of this user can `connect(2)` to, so
/// the `agent_id` is checked against a secret the supervisor minted when it claimed that node. A
/// build that stopped sending the token, or a supervisor that stopped checking it, fails here.
///
/// **Three assertions, because a refusal that creates something is not a refusal**: the answer is an
/// error, the journal grew by nothing, and no child process exists by the children's own account.
/// The `agent_id` is the *real* one, so the only thing wrong with this call is the proof — which is
/// what keeps this test about the token rather than about a malformed frame.
#[test]
fn a_spawn_presenting_a_forged_node_token_is_refused_and_journals_nothing() {
    let fx = fixture("bg-forged-token");
    let before = fx.journal_text().lines().count();
    let mut bridge = fx.bridge_with_a_forged_token();

    // **`background: true`, so a served spawn is a *fast* failure rather than a slow one.** A
    // refusal comes back in the same frame either way, but if this mutation-checks green — a
    // supervisor that stopped comparing tokens — a synchronous call would be *served*, and the test
    // would then hang on a gated child until the deadlock bound and fail on the clock instead of on
    // the claim. A test that can only fail by timing out is not a test.
    let reply = bridge.tool("spawn", spawn_args(true));
    assert!(
        is_error(&reply),
        "a caller that cannot prove who it is may not spawn: {reply}"
    );
    let text = text_of(&reply);
    assert!(
        text.contains("did not mint that node token"),
        "the supervisor's own sentence reaches the caller verbatim: {text}"
    );
    assert_eq!(
        fx.journal_text().lines().count(),
        before,
        "an unauthorized spawn is refused before every side effect, so the journal records nothing"
    );
    assert_eq!(
        fx.started_children(),
        0,
        "and no child process ever ran, by the children's own account"
    );
    assert!(bridge.close().success());
}

/// **A launch that fails before a process exists journals no `Spawned` record at all** — the other
/// half of item 28 step 1's claim, and the one that keeps `abandoned()` honest.
///
/// The pid assertion above says a record that *is* written names a live process. This says the
/// record is not written when there is nothing to name. Together they are what makes
/// `SpawnIntent`-and-nothing-after mean **no process exists, full stop** — §11 item 30's shapes 1
/// and 2 ceasing to be indistinguishable on disk.
///
/// The failure is now the supervisor's to report and the bridge's to carry: the harness is
/// unlaunchable on the *supervisor's* `PATH`, `agent/spawn` never sees a process, and the sentence
/// travels back over the socket rather than being formed where the child would have run.
#[test]
fn a_launch_that_fails_before_the_process_exists_journals_no_spawned_record() {
    let fx = fixture("bg-launch-fails");
    fx.break_the_harness();
    let mut bridge = fx.bridge();

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
        fx.child_spawned_pids(),
        Vec::new(),
        "and no `Spawned` record was written about a child, so the intent alone means what it now \
         claims to mean: there is no process and there never was one"
    );
    let journal = fx.journal_text();
    assert!(
        journal.contains("SpawnAborted"),
        "the intent is still resolved as an abort — §7.2: a node marion decided the fate of is \
         never one marion lost: {journal}"
    );
    assert!(bridge.close().success());
}

/// **A second `wait` on the same handle says so, rather than blocking forever.**
///
/// The other half of the handle's contract. The mistake this guards has changed shape with the
/// path: a `wait` is now an attach-and-read, so a second one would happily replay the node's whole
/// stream and hand back the same contract a second time — which reads as a second run. The table
/// remembers what the first `wait` got precisely so the second can be told the truth.
#[test]
fn a_collected_handle_cannot_be_collected_twice() {
    let fx = fixture("bg-collect-twice");
    let mut bridge = fx.bridge();
    let task_id = handle_task_id(&bridge.tool("spawn", spawn_args(true)));
    fx.open_gate();
    // The shim never calls `report`, so this contract is `Unreported` and therefore `isError:
    // true` — `spawn_result`'s deliberate shape for a child that produced no answer, and not what
    // this test is about. What matters is that the contract came back at all.
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

/// **`max_concurrent_children` refuses rather than queues** — §3.1.
///
/// **The count now comes from the registry, not from this bridge's table**, which is the inversion
/// §11 item 28 step 4 made and step 5 finished: `handler::live_children_of` counts the caller's
/// non-terminal children in the journal, so the bound holds across bridges, across restarts, and
/// against a caller that cannot state it. This test cannot tell those two implementations apart on
/// its own — that is `handler`'s own
/// `the_concurrency_gate_counts_the_callers_children_in_the_journal` — and what it does assert is
/// that the bound is live on the surface a model actually calls.
///
/// **Refuses, not queues, and the difference is observable here rather than argued.** A queueing
/// implementation would answer the fifth `spawn` with a handle and start the child once a slot
/// freed; this asserts the fifth comes back an **error, in the same frame**, while all four
/// predecessors are still blocked on the gate — and that the refusal **names the bound**, since
/// §3.1 requires a refusal that says what it refused.
#[test]
fn the_fifth_concurrent_background_child_is_refused_in_the_same_frame() {
    let fx = fixture("bg-concurrency");
    let mut bridge = fx.bridge();
    let max = marion_core::agent_type::builtin(CALLER_TYPE)
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
    fx.await_children(max as usize);
    assert_eq!(
        fx.started_children(),
        max as usize,
        "the refused spawn started no process: a refusal creates nothing (§6.1 step 2 runs before \
         every side effect)"
    );

    fx.open_gate();
    assert!(bridge.close().success());
}

/// **A collected child frees the slot it was holding** — §3.1 bounds concurrency, not a lifetime.
///
/// The regression this was written for was `Background::live_children` counting rows that `wait`
/// never removed: four children started, finished and collected refused the fifth for the life of
/// the bridge. The count is the registry's now, and the same property has to hold for a different
/// reason — a child whose `Exited` record is on disk is not a live child — so the test survives the
/// move and its subject changed underneath it. It says nothing about the *uncollected* case, which
/// is a slot genuinely still occupied.
#[test]
fn a_collected_child_frees_the_concurrency_slot_it_was_holding() {
    let fx = fixture("bg-slot-release");
    let mut bridge = fx.bridge();
    let max = marion_core::agent_type::builtin(CALLER_TYPE)
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
        !text_of(&again).contains("max_concurrent_children"),
        "with every child terminal there is nothing to be concurrent with, so the refusal that \
         must not happen is the concurrency one: {}",
        text_of(&again)
    );
    assert!(bridge.close().success());
}

/// **`allow_concurrent_writes: false` is honoured structurally** — no two live children share a
/// cwd, because each one gets its own worktree.
///
/// **It is also the git-serialization control, and a weak one — see `spawn::repo_write_guard`.**
/// Concurrent `git worktree add`/`remove` against one repository really does fail, but S17 measured
/// the failure in `.git/worktrees/` bookkeeping rather than in `index.lock`, and measured its rate:
/// at four concurrent writers it needs a few hundred iterations to appear. This test performs four
/// `worktree add`s **once**, so it is three orders of magnitude short of witnessing it. Read it as
/// what it reliably is — **every** concurrent child got its own workspace and none was lost.
///
/// The writers are all one process now (the supervisor) where they used to be one process too (the
/// bridge), so the guard's subject is unchanged by step 5.
#[test]
fn concurrent_children_never_share_a_workspace_and_never_lose_one_to_a_git_lock() {
    let fx = fixture("bg-worktrees");
    let mut bridge = fx.bridge();
    let max = marion_core::agent_type::builtin(CALLER_TYPE)
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
        let contract: Value = serde_json::from_str(
            &text[text
                .find('{')
                .unwrap_or_else(|| panic!("a contract is json: {text}"))..],
        )
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

/// **A `timeout_secs` the machine's clock cannot represent is answered, not detonated** — B1, the
/// synchronous half.
///
/// `timeout_secs` is caller-controlled and typed `u64` all the way down, and `Instant + Duration`
/// **panics** on overflow — after `command.spawn()` has already started a real OS process.
///
/// **Whose process dies has changed, and the number has one more clock to survive now.** The panic
/// used to land on the bridge's own dispatch thread and take the MCP server down with it; it would
/// now land on a node's thread inside the *supervisor*, which is worse — one child's arithmetic
/// would take out the fleet. And the bridge derives its own waiting bound from the same field, so an
/// unclamped `u64::MAX` there would be a second overflow on this side of the socket. The assertion
/// stays the crudest possible one, deliberately: **both processes are still there and the caller was
/// answered.**
///
/// The gate is opened before the spawn because this path is synchronous: the reply cannot arrive
/// until the child exits.
#[test]
fn a_timeout_the_clock_cannot_represent_is_answered_rather_than_killing_the_bridge() {
    let fx = fixture("bg-overflow-sync");
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

    // **The clamp is recorded, not silently substituted.** §6.7's rule everywhere else is that the
    // contract names the compiled value rather than the asked-for one.
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
/// Before the fix: `run_spawn` journals `SpawnIntent`, makes the worktree, **starts a real
/// process**, panics one line later on `Instant::now() + Duration::from_secs(u64::MAX)`,
/// `AbortOnDrop` writes `SpawnAborted` — the record that says *marion decided this node's fate* —
/// and dropping `std::process::Child` kills nothing, so the process it started is still there. The
/// journal reads as a decided node while a live process and its whole workspace remain.
#[test]
fn a_backgrounded_timeout_the_clock_cannot_represent_resolves_its_intent_rather_than_aborting_it() {
    let fx = fixture("bg-overflow-bg");
    let mut bridge = fx.bridge();

    let reply = bridge.tool("spawn", spawn_args_with_timeout(true, u64::MAX));
    assert!(
        !is_error(&reply),
        "a backgrounded spawn that started is not an error: {reply}"
    );
    let task_id = handle_task_id(&reply);

    fx.await_children(1);
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

/// **A harness that hangs answering `--version` does not hang anybody** — B3's root cause, one
/// process further out.
///
/// The probe used a plain `Command::output`, which blocks until the process closes its pipes and
/// has no deadline at all, and §11 item 28 step 1 moved it *ahead* of the launch (§6.1 step 3). So
/// an unbounded probe would now hold `agent/spawn` itself rather than a `wait` — the same bug with
/// a wider blast radius, since the call it blocks is on the supervisor's socket.
///
/// The last two assertions are the other half: the bridge is still reading frames afterwards, which
/// a `wait`-only assertion would miss.
///
/// The absent version is recorded as *absent*, never guessed: a node's whole contract must not be
/// lost because a version string did not arrive, and a fabricated one would be worse than either.
#[test]
fn a_harness_that_hangs_answering_its_version_does_not_hang_the_bridge() {
    let fx = fixture("bg-slow-version");
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

    let after = bridge.tool("wait", json!({"task_id": "task-nobody-started"}));
    assert!(
        is_error(&after) && !text_of(&after).is_empty(),
        "the bridge still answers frames after a child overran its version probe: {after}"
    );
    assert!(bridge.close().success());
}

// ---------------------------------------------------------------------------------------------
// §5.4's read verbs — `status` and `list` — against a real supervisor
// ---------------------------------------------------------------------------------------------

/// **A real MCP handshake lists exactly the tools marion declares — no more, no fewer.**
///
/// §9 records the tool count deliberately (*"the root's turns carry `ntools=2`… recorded so a later
/// reader does not diagnose it as a tool-compilation failure"*), and an `ntools` mismatch is a
/// failure mode this repository has actually been bitten by: an allowlist naming four verbs while
/// the bridge declared two, for months, with nothing on either side saying so.
///
/// The assertion is on the **set and the count**, over a genuine `initialize` + `tools/list`
/// exchange with a bridge process started from the declaration marion itself wrote. Three things it
/// pins that a unit test on `bridge::tools()` cannot: the list survives serialization to the wire,
/// it arrives in answer to the frame a real client sends, and the count is what an operator reading
/// `ntools` will see.
///
/// **`ROOT_VERBS` is checked against it in the same test, in both directions.** An
/// allowlist entry for an undeclared tool is inert — §9 says so and blessed it — and a *declared*
/// tool missing from the allowlist is the converse defect, which is not inert at all: it is offered
/// to the root and then denied on use, and the denial spends the root's bound. Neither can now
/// happen silently.
#[test]
fn a_real_handshake_lists_exactly_the_tools_marion_declares() {
    let fx = fixture("bg-tool-surface");
    let mut bridge = fx.bridge();

    let listed = bridge.call("tools/list", json!({}));
    let tools = listed["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list answers with a list: {listed}"));
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    assert_eq!(
        names,
        vec!["spawn", "wait", "status", "list", "report"],
        "the declared surface, over a real handshake"
    );
    // Stated as a number as well as a list, because `ntools` is what the harness logs and what a
    // reader diagnoses from — and because a count is the one thing a careless merge of two
    // declarations gets wrong while keeping every name.
    assert_eq!(
        names.len(),
        5,
        "§9's ntools, as a real client would count it"
    );

    // Every tool declares a schema a client can compile. A name with no `inputSchema` is offered
    // and then unusable, which reads to a model exactly like a tool that does not work.
    for t in tools {
        assert_eq!(
            t["inputSchema"]["type"], "object",
            "every declared tool carries an object schema: {t}"
        );
        assert!(
            t["description"].as_str().is_some_and(|d| !d.is_empty()),
            "every declared tool says what it is for: {t}"
        );
    }

    // The allowlist and the declared surface, in both directions.
    let allowed: Vec<String> = marion_supervisor::root::ROOT_VERBS
        .iter()
        .map(|t| t.to_string())
        .collect();
    for verb in &allowed {
        assert!(
            names.contains(&verb.as_str()),
            "`{verb}` is permitted to a root but not declared, so the allowlist entry is inert"
        );
    }
    for name in &names {
        assert!(
            allowed.contains(&name.to_string()) || *name == "report",
            "`{name}` is declared but not permitted to a root, so a root's call is offered and \
             then denied — spending its bound. `report` is the one deliberate exception (§5.4 \
             rejects it on a root, and `REPORT_ON_A_ROOT` is the sentence that says so)."
        );
    }
    assert!(bridge.close().success());
}

/// **`status` answers from the supervisor, not from the row this bridge stored at spawn.**
///
/// This is the staleness control, and it is built so that a cached answer cannot pass it. The child
/// is backgrounded — so the handle is handed out while the node is still starting — then the gate
/// is opened and the child is driven to a terminal state. A `status` served from
/// `background::Handed` would report whatever was true at hand-out time and would report it
/// forever, with `isError: false`; only a `status` that asks `node/get` can name the terminal
/// state, because the terminal state is a fact that came into existence after the handle did.
///
/// **The two observations are the same handle at two times**, which is what makes the assertion
/// about freshness rather than about correctness at one instant. A first `status` before the gate
/// must not already say "finished", and a later one must — so an implementation that hardcoded
/// either answer fails one of them.
#[test]
fn status_reports_the_supervisors_current_state_and_not_a_cached_one() {
    let fx = fixture("bg-status-fresh");
    let mut bridge = fx.bridge();
    let task_id = handle_task_id(&bridge.tool("spawn", spawn_args(true)));

    // Before the gate: the child is alive, blocked in the shim. Whatever this says, it must not be
    // that the child has finished — it has not.
    let early = bridge.tool("status", json!({"task_id": &task_id}));
    let early_text = text_of(&early);
    assert!(
        !is_error(&early),
        "a status on a live child of this bridge resolves: {early_text}"
    );
    assert!(
        early_text.contains(&task_id),
        "the answer names the handle the caller addressed it by: {early_text}"
    );
    assert!(
        !early_text.contains("finished"),
        "the child is blocked on the gate and has not finished: {early_text}"
    );

    // Drive it to a terminal state through the ordinary path, then collect, so the node really is
    // `Exited` in the supervisor's registry.
    fx.open_gate();
    let collected = bridge.tool("wait", json!({"task_id": &task_id}));
    assert!(
        text_of(&collected).contains("\"completion\""),
        "the child reached a terminal state: {collected}"
    );

    // The same handle, after. This is the assertion a cache cannot satisfy.
    //
    // **Polled to the supervisor's own barrier rather than read once.** `wait` and `status` are
    // two observers of one death and nothing orders them: `wait` returns the moment the node's own
    // event stream carries its closing bookend (`courier::await_contract` reads `node/attach`
    // notifications), while `status` reads the registry the supervisor projects out of the journal
    // it is *tailing* — so the registry is behind by however long that tail takes to notice. On a
    // loaded ubuntu runner it was behind by enough to still be reporting `Spawning`, the state at
    // hand-out time, which reads exactly like the cache this test exists to rule out.
    //
    // Waiting for the terminal answer weakens nothing, because the two candidate explanations
    // diverge rather than converge: a `status` served from `background::Handed` would report
    // `Spawning` **for ever**, so no amount of polling turns a cached answer into "finished".
    // [`DEADLOCK_BOUND`] is therefore still a deadlock bound and not a timing assertion — reaching
    // it means the registry never learnt, which is the bug.
    let deadline = Instant::now() + DEADLOCK_BOUND;
    let (late, late_text) = loop {
        let reply = bridge.tool("status", json!({"task_id": &task_id}));
        let text = text_of(&reply);
        if text.contains("finished") || Instant::now() >= deadline {
            break (reply, text);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        !is_error(&late),
        "§5.4 permits `status` against a terminal target, and against one an earlier `wait` has \
         already collected — a handle stops being collectable, not readable: {late_text}"
    );
    assert!(
        late_text.contains("finished"),
        "the node is terminal in the supervisor's registry and `status` must say so — an answer \
         cached at hand-out time could not: {late_text}"
    );
    assert_ne!(
        early_text, late_text,
        "the same handle read twice across the child's terminal transition must not give the same \
         answer; if it does, nothing was asked of the supervisor"
    );
    assert!(bridge.close().success());
}

/// **`list` answers from the supervisor and shows this caller's children — and only those.**
///
/// Two spawns, so the count is not satisfiable by an implementation that returns the first thing it
/// finds, and the fixture's own **root** is asserted absent: the root is this bridge's node, it is
/// in the supervisor's `tree/subscribe` answer, and §5.4 scopes `list` to *descendants*. A `list`
/// that forwarded the wire's node list unfiltered would include it — along with every other root in
/// the project — and that is the authorization defect this assertion exists to catch, not a
/// cosmetic one.
#[test]
fn list_shows_this_callers_children_and_not_the_node_doing_the_asking() {
    let fx = fixture("bg-list");
    let mut bridge = fx.bridge();

    let empty = bridge.tool("list", json!({}));
    assert!(
        !is_error(&empty),
        "a caller that has delegated nothing gets an answer, not an error: {empty}"
    );
    assert!(
        text_of(&empty).contains("no child agents"),
        "and is told the tree is empty in words rather than handed an empty array: {}",
        text_of(&empty)
    );

    let first = handle_task_id(&bridge.tool("spawn", spawn_args(true)));
    let second = handle_task_id(&bridge.tool("spawn", spawn_args(true)));
    assert_ne!(first, second, "two spawns, two handles");

    let listed = bridge.tool("list", json!({}));
    let text = text_of(&listed);
    assert!(!is_error(&listed), "list resolves: {text}");
    assert!(
        text.contains("2 child agents"),
        "both children appear, counted: {text}"
    );
    assert!(
        !text.contains(&fx.root_id.0),
        "the caller is this bridge's own node and §5.4 scopes `list` to descendants; a `list` that \
         names the asker is forwarding the wire's whole node list unfiltered: {text}"
    );

    fx.open_gate();
    assert!(bridge.close().success());
}
