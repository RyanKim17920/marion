//! **`marion mcp` — marion as an MCP server for a client marion did not start.**
//!
//! Driven through the **real `marion` binary over stdio**, the way an MCP client drives it: a
//! process started from a config file, spoken to in JSON-RPC on its stdin, answering on its stdout.
//! Nothing here reaches into the library; every assertion is about what came back down the pipe.
//!
//! # What this file is for
//!
//! Since §11 item 28 marion is a supervisor plus clients, and the clients are peers. `marion run`
//! is one. This is the other, and the whole risk in adding it is that a *second* top-level surface
//! is a second place a launch path can grow — which is the exact defect item 28 removed. So the
//! tests below are shaped around two questions:
//!
//! 1. **Does it own anything?** It must not. A root it creates is the supervisor's node: it
//!    survives this process's death, and this process's `spawn` reached it over the socket.
//! 2. **Does it declare what it does?** The tool surface is answered over a genuine handshake and
//!    compared to `bridge::tools()` by set *and* by count, because §9 records `ntools` and an
//!    `ntools` mismatch is a failure mode this repository has been bitten by.
//!
//! The supervisor-on-demand decision (`mcp::Principal::ensure_supervisor`) gets both of its
//! directions measured here, because it is the one behaviour the two MCP surfaces genuinely do
//! differently and an argument in a doc comment is not a check.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test top_level_mcp
//! ```
//!
//! It needs **no harness binary, no network and no credential**: a shim replaces `codex`, and the
//! shim is what every root here actually runs.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use marion_supervisor::socket::{project_root, socket_paths};
use marion_testsupport::{Scratch, fixture_repo, scratch, survivors, sweep};
use serde_json::{Value, json};

/// A bound that exists **only to fail**, never to be reached on a passing run. Nothing here asserts
/// on elapsed time; reaching this means something is genuinely blocked.
const BOUND: Duration = Duration::from_secs(90);

/// The shim's own hard lifetime, so no bad run can strand one.
const SHIM_LIFE: Duration = Duration::from_secs(60);

/// The root's wall clock. Comfortably longer than [`SHIM_LIFE`], so the gate — never a timeout — is
/// what ends a root on a passing run.
const ROOT_TIMEOUT_SECS: u64 = 120;

/// How long a "nothing happened" observation samples for. **Not a correctness bound**: the thing
/// being watched for would happen within milliseconds if it happened at all.
const QUIET_SAMPLE: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------------------------
// The bed
// ---------------------------------------------------------------------------------------------

fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"))
}

/// A `codex` that answers `--version` instantly and then blocks until the test opens the gate.
///
/// Mortal by construction: the gate lives in a [`Scratch`] the fixture removes, so a shim that only
/// ever waited would spin forever if a test panicked. The cap is shorter than [`ROOT_TIMEOUT_SECS`]
/// so it is never what ends a passing run.
fn shim(dir: &Path, gate: &Path, started: &Path) {
    let bin = dir.join("codex");
    let script = format!(
        r#"#!/bin/sh
case "$1" in
  --version) echo "codex-cli 0.146.0-marion-shim"; exit 0 ;;
esac
mkdir -p {started}
: > {started}/$$
waited=0
while [ ! -e {gate} ]; do
  sleep 0.05
  waited=$((waited + 1))
  if [ "$waited" -gt {ticks} ]; then exit 0; fi
done
exit 0
"#,
        started = shell_quote(started),
        gate = shell_quote(gate),
        ticks = SHIM_LIFE.as_millis() / 50,
    );
    std::fs::write(&bin, script).expect("the shim is written");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
        .expect("the shim is executable");
}

struct Fixture {
    _scratch: Scratch,
    repo: PathBuf,
    state: PathBuf,
    gate: PathBuf,
    started: PathBuf,
    path_env: String,
    needle: String,
}

fn fixture(tag: &str) -> Fixture {
    let s = scratch(tag);
    let dir = s.to_path_buf();
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    let shim_dir = dir.join("bin");
    let gate = dir.join("gate");
    let started = dir.join("started");
    std::fs::create_dir_all(&shim_dir).expect("the shim dir");
    std::fs::create_dir_all(&state).expect("the state dir");
    shim(&shim_dir, &gate, &started);
    // **The whole search path is stated, not extended** — `execvp` skips a `PATH` entry whose
    // `codex` fails to exec and keeps going, so an extended `PATH` could reach a real harness.
    for d in ["/usr/bin", "/bin"] {
        assert!(
            !Path::new(d).join("codex").exists(),
            "{d} holds a `codex`, so this bed could launch a real harness and assert nothing"
        );
    }
    Fixture {
        needle: dir.to_string_lossy().to_string(),
        path_env: format!("{}:/usr/bin:/bin", shim_dir.to_string_lossy()),
        _scratch: s,
        repo,
        state,
        gate,
        started,
    }
}

impl Drop for Fixture {
    /// Open the gate so every shim ends the ordinary way, then sweep — including the supervisor
    /// `marion mcp` started, which is keyed on this fixture's own scratch directory and so is
    /// named by the needle.
    fn drop(&mut self) {
        let _ = std::fs::write(&self.gate, b"go");
        let deadline = Instant::now() + QUIET_SAMPLE;
        while Instant::now() < deadline && !survivors(&self.needle).is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let left = sweep(&self.needle);
        assert!(
            left.is_empty(),
            "this fixture left live processes behind: {left:?}"
        );
    }
}

impl Fixture {
    /// §2's socket for this fixture's project — derived the way a client derives it, so the test
    /// and `marion mcp` cannot disagree about which supervisor "listening" means.
    fn socket(&self) -> PathBuf {
        // SAFETY: reads the calling process's real uid and cannot fail.
        let uid = unsafe { getuid() };
        socket_paths(&self.state, &project_root(&self.repo), uid)
            .socket()
            .to_path_buf()
    }

    fn nothing_is_listening(&self) -> bool {
        std::os::unix::net::UnixStream::connect(self.socket()).is_err()
    }

    /// Let every shim finish. The same write [`Fixture::drop`] does, performed early by a test that
    /// needs the root to reach a terminal state while it is still watching.
    fn open_gate(&self) {
        std::fs::write(&self.gate, b"go").expect("the gate opens");
    }

    /// `marion mcp`, started the way an MCP client's config starts it.
    fn server(&self) -> Server {
        Server::start(self, &[])
    }
}

unsafe extern "C" {
    fn getuid() -> u32;
}

// ---------------------------------------------------------------------------------------------
// The server under test
// ---------------------------------------------------------------------------------------------

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Option<String>>,
    next_id: i64,
}

impl Server {
    fn start(fx: &Fixture, extra: &[&str]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_marion"));
        cmd.arg("mcp")
            .arg("--repo")
            .arg(&fx.repo)
            .arg("--state-dir")
            .arg(&fx.state)
            // Nothing here reaches it: every harness invocation is the shim. Stated rather than
            // defaulted because the default is the operator's own login, which a test must never
            // reach for.
            .arg("--canned")
            .args(extra)
            // The supervisor this server starts inherits this environment, and the supervisor is
            // what `exec`s a harness — so this is the `PATH` that decides which `codex` runs.
            .env("PATH", &fx.path_env)
            .env("HOME", fx.state.parent().unwrap_or(&fx.state));
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the marion binary starts");
        let stdin = Some(child.stdin.take().expect("piped"));
        let mut stdout = BufReader::new(child.stdout.take().expect("piped"));
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            loop {
                let mut line = String::new();
                match stdout.read_line(&mut line) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(None);
                        return;
                    }
                    Ok(_) => {
                        if !line.trim().is_empty() && tx.send(Some(line)).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            next_id: 1,
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let w = self.stdin.as_mut().expect("stdin is still open");
        writeln!(w, "{frame}").expect("the server is still reading");
        w.flush().expect("flushed");
        match self.lines.recv_timeout(BOUND) {
            Ok(Some(line)) => serde_json::from_str(line.trim())
                .unwrap_or_else(|e| panic!("the reply to {method} is not json: {e}: {line}")),
            Ok(None) => panic!("the server closed its stdout instead of answering {method}"),
            Err(_) => panic!("no reply to {method} within {BOUND:?}"),
        }
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.call("tools/call", json!({"name": name, "arguments": arguments}))
    }

    /// The text of a `tools/call` answer, whichever way it went.
    fn text(answer: &Value) -> String {
        answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("a tool answer carries text: {answer}"))
            .to_string()
    }

    fn close(mut self) -> std::process::ExitStatus {
        self.stdin.take();
        self.child.wait().expect("the server exits at EOF")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A root-creating `spawn`, as a top-level client's model would send one.
fn spawn_args(background: bool) -> Value {
    json!({
        "agent_type": "codex",
        "prompt": "block until the gate opens",
        "acceptance_criteria": ["the shim exits 0"],
        "timeout_secs": ROOT_TIMEOUT_SECS,
        "background": background,
    })
}

/// Wait until `f` holds, or fail naming what never happened.
fn until(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + BOUND;
    while !f() {
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------------------------
// The declared surface
// ---------------------------------------------------------------------------------------------

/// **A real MCP handshake against `marion mcp` lists exactly the tools marion declares.**
///
/// The same assertion `background_spawn.rs` makes about the per-child bridge, made about the
/// top-level surface, and it is not redundant: the two are separate processes reached by separate
/// argv, and the failure this guards is *one* of them drifting. §9 records the tool count
/// deliberately, and an `ntools` mismatch is a failure mode this repository has actually been
/// bitten by — an allowlist naming four verbs while the bridge declared two, for months.
///
/// **The list is compared against `bridge::tools()` itself, not against a literal repeated here.**
/// A literal would pass a change that renamed a tool in both places and would still not tell you
/// the wire agreed with the declaration; comparing to the source is what makes this a statement
/// about serialization and dispatch rather than about a constant.
///
/// It also pins the handshake itself — `initialize` answers with marion's `serverInfo` — because a
/// server that never completes one is a server no client will send a `tools/call` to, and every
/// other test in this file would then be measuring nothing.
#[test]
fn a_real_handshake_against_the_top_level_server_lists_exactly_the_tools_marion_declares() {
    let fx = fixture("mcp-top-tools");
    let mut s = fx.server();

    let init = s.call("initialize", json!({}));
    assert_eq!(
        init["result"]["serverInfo"]["name"], "marion",
        "the handshake completes and names marion: {init}"
    );

    let listed = s.call("tools/list", json!({}));
    let tools = listed["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list answers with a list: {listed}"))
        .clone();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    let declared = marion_supervisor::bridge::tools();
    let expected: Vec<&str> = declared
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names, expected,
        "the wire's tool list is marion's own declaration, in order"
    );
    assert_eq!(
        names.len(),
        expected.len(),
        "§9's ntools, as a real client would count it"
    );
    for t in &tools {
        assert_eq!(
            t["inputSchema"]["type"], "object",
            "every declared tool carries a schema a client can compile: {t}"
        );
        assert!(
            t["description"].as_str().is_some_and(|d| !d.is_empty()),
            "every declared tool says what it is for: {t}"
        );
    }

    // **Nothing but frames reached stdout.** stdout is the protocol here, and a stray `println!`
    // anywhere on the startup path would be handed to a client's JSON parser. Every line read so
    // far parsed as JSON — `call` panics otherwise — so this asserts the *count*: two requests, two
    // replies, nothing volunteered in between.
    assert!(
        s.lines.try_recv().is_err(),
        "the server wrote something to stdout that nobody asked for"
    );
    assert!(s.close().success());
}

/// **Every tool `marion mcp` declares is dispatched, and the two that a top-level client may not
/// use are refused by name rather than served.**
///
/// This is the repository's standing rule at the top-level surface: a verb marion advertises and
/// does not perform must **refuse by name**, never fall through to `no tool …` (which says it does
/// not exist, contradicting the `tools/list` marion just sent) and never answer as though it had
/// worked (which is §12's accept-and-ignore, the shape this codebase keeps legislating against).
///
/// `report` is the case that only exists up here. It is declared to every MCP surface, and for a
/// top-level client it is not merely disallowed but **inapplicable**: there is no node, so there is
/// no depth to evaluate §5.4 against and no contract a payload could be staged into. Answering
/// `report recorded` — which is what a dispatch that forgot this principal would do, since that is
/// the per-node arm's success path — would be a receipt for a payload nothing stages, handed to a
/// client that would then stop looking for a way to return its result.
#[test]
fn every_tool_the_top_level_server_declares_is_dispatched_and_report_is_refused_by_name() {
    let fx = fixture("mcp-top-dispatch");
    let mut s = fx.server();
    s.call("initialize", json!({}));

    let declared = marion_supervisor::bridge::tools();
    for t in declared.as_array().unwrap() {
        let name = t["name"].as_str().unwrap();
        // `spawn` would start a root; every other verb is a read and costs nothing. What is
        // asserted is that the name was **routed**, not that the call succeeded.
        if name == "spawn" {
            continue;
        }
        let answer = s.tool(name, json!({}));
        let text = Server::text(&answer);
        assert!(
            !text.contains("no tool"),
            "`{name}` is in this server's own tools/list and answered as though it were not: \
             {text:?}"
        );
    }

    // The one that must refuse, and must refuse *as itself*.
    let reported = s.tool("report", json!({"narrative": "I did the thing"}));
    assert_eq!(
        reported["result"]["isError"],
        json!(true),
        "a top-level client's `report` is refused: {reported}"
    );
    let text = Server::text(&reported);
    assert!(
        text.contains("not a node"),
        "and the refusal says why this client in particular cannot report, rather than reusing the \
         root's sentence — a top-level client occupies no position in the tree at all: {text}"
    );
    assert!(
        !text.contains("report recorded"),
        "and never issues a receipt for a payload nothing stages: {text}"
    );
    assert!(
        !text.contains("This node is the root"),
        "and does not claim to be a node marion started, which is a different fact: {text}"
    );

    // The converse, so the sweep above cannot be satisfied by deleting the fallthrough.
    let unknown = s.tool("teleport", json!({}));
    assert!(
        Server::text(&unknown).contains("no tool teleport"),
        "an undeclared tool is still refused by name"
    );
    assert!(s.close().success());
}

// ---------------------------------------------------------------------------------------------
// §5.7's on-demand start — the one decision the two MCP surfaces make differently
// ---------------------------------------------------------------------------------------------

/// **`marion mcp` starts a supervisor on demand, and only when something is demanded of it.**
///
/// Two halves, and the second is the one that is easy to get wrong.
///
/// **It starts one**, because it is §5.7's *"first client that dials the §2 socket path and finds
/// nothing listening"* — the same position `marion run` is in. The per-child bridge refuses in this
/// situation and that is not an inconsistency: a bridge holds a token a *dead* supervisor minted,
/// so a fresh one would refuse its very next call anyway, and the node it serves is gone with its
/// owner. A top-level client holds no token and names a repository, so nothing it presents can be
/// stale. `background_spawn.rs`'s
/// `a_spawn_whose_supervisor_is_not_listening_is_refused_and_starts_nothing` is the other half of
/// this pair, and the two together are the decision.
///
/// **It does not start one at boot.** An MCP client launches every server in its config at session
/// start, so a `marion mcp` that dialed on startup would leave a supervisor resident per editor
/// window that had marion configured, for people who never spawned anything. §5.7's demand is a
/// client that *wants something*. The handshake and a read verb are both performed here before the
/// spawn, and neither may bring one up — which also means a `list` against an empty project answers
/// "nothing" instead of manufacturing the thing it was asked to report on.
#[test]
fn the_top_level_server_starts_a_supervisor_when_a_spawn_needs_one_and_not_before() {
    let fx = fixture("mcp-top-ondemand");
    assert!(
        fx.nothing_is_listening(),
        "the bed starts with no supervisor, which is the whole precondition"
    );
    let mut s = fx.server();

    s.call("initialize", json!({}));
    s.call("tools/list", json!({}));
    // A read verb, which must report rather than create. Its answer is a refusal — there is nothing
    // to read — and that refusal is the correct one.
    let listed = s.tool("list", json!({}));
    assert_eq!(
        listed["result"]["isError"],
        json!(true),
        "a `list` with no supervisor says so: {listed}"
    );

    let deadline = Instant::now() + QUIET_SAMPLE;
    while Instant::now() < deadline {
        assert!(
            fx.nothing_is_listening(),
            "starting the server and reading from it brought a supervisor up; §5.7's demand is a \
             client that wants something, not a client that exists"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Now demand something.
    let spawned = s.tool("spawn", spawn_args(true));
    assert_eq!(
        spawned["result"]["isError"],
        json!(false),
        "a backgrounded root spawn is answered: {spawned}"
    );
    until("a supervisor bound the socket", || {
        !fx.nothing_is_listening()
    });
    until("the root's harness really launched", || {
        std::fs::read_dir(&fx.started).is_ok_and(|d| d.count() > 0)
    });
}

/// **The root `marion mcp` creates is the supervisor's node, not this server's child.**
///
/// This is the peer invariant measured rather than scanned for. `mcp::tests::the_mcp_entry_point_has_no_spawn_path_of_its_own`
/// reads the source and asserts an absence; this asserts the consequence, which is the thing
/// anybody actually cares about: **kill the MCP server and the agent keeps running.**
///
/// The assertion is on *liveness*, not on presence in the process table — the shim's `started`
/// marker exists before the kill, so a zombie would satisfy a presence check. What is required
/// after the kill is that the node is still there to be *asked about*, over the socket, by a client
/// that is not this one. A server that had launched the root in-process would take it down with
/// itself, and a `tree/subscribe` from anywhere would then show nothing.
///
/// It is also why `spawn` is backgrounded: a synchronous one would not return until the root
/// finished, and there would be no window in which to kill anything.
#[test]
fn killing_the_top_level_server_leaves_its_root_running_under_the_supervisor() {
    use marion_proto::params::TreeSubscribeParams;
    use marion_proto::{Call, Frame, Method, MethodResult, Outcome, Request, RequestId};

    let fx = fixture("mcp-top-owns-nothing");
    let mut s = fx.server();
    s.call("initialize", json!({}));
    let spawned = s.tool("spawn", spawn_args(true));
    assert_eq!(
        spawned["result"]["isError"],
        json!(false),
        "a backgrounded root spawn is answered: {spawned}"
    );
    until("the root's harness really launched", || {
        std::fs::read_dir(&fx.started).is_ok_and(|d| d.count() > 0)
    });

    // SIGKILL, not a close: an EOF would let the server leave tidily, and what is under test is
    // what happens when it does not get to.
    let pid = s.child.id();
    marion_testsupport::kill_hard(pid as i32);
    drop(s);

    // A different client entirely, on a connection this test opens itself.
    let mut c = std::os::unix::net::UnixStream::connect(fx.socket())
        .expect("the supervisor outlived the client that started it");
    c.set_read_timeout(Some(BOUND)).unwrap();
    let frame = Frame::Request(Request::new(
        RequestId::Number(1),
        Call::TreeSubscribe(TreeSubscribeParams {}),
    ));
    c.write_all(frame.to_line().as_bytes()).unwrap();
    c.flush().unwrap();
    let mut r = BufReader::new(c.try_clone().unwrap());
    let answered = loop {
        let mut line = String::new();
        assert!(
            r.read_line(&mut line).expect("a frame arrives") > 0,
            "the supervisor closed the connection without answering"
        );
        if let Frame::Response(resp) = Frame::from_line(&line).expect("well-formed") {
            match resp.outcome {
                Outcome::Result(v) => break v,
                Outcome::Error(e) => panic!("tree/subscribe was refused: {}", e.message),
            }
        }
    };
    let MethodResult::TreeSubscribe(tree) = Method::TreeSubscribe
        .decode_result(&answered)
        .expect("a readable tree/subscribe result")
    else {
        panic!("tree/subscribe answers with a tree/subscribe result");
    };
    assert_eq!(
        tree.nodes.len(),
        1,
        "the root this server asked for is still the supervisor's node after the server was \
         killed: {:?}",
        tree.nodes
    );
    assert!(
        tree.nodes[0].parent_id.is_none(),
        "and it is a root — `caller: None` is what makes a top-level `spawn` create one: {:?}",
        tree.nodes[0]
    );
    // **Both disjuncts, because neither alone is "still going".** `NodeState::is_exited` is one
    // half of §7.6's terminal rule and `ReapState::is_terminal_for_gating` is the other; a node
    // killed with its server would land in *one* of them, and asserting only the first would pass
    // for a node marion had marked `Orphaned` — which is precisely the outcome an in-process
    // launch path would produce.
    assert!(
        !tree.nodes[0].state.is_exited(),
        "and it is still going, which is the claim: {:?}",
        tree.nodes[0]
    );
    assert!(
        !tree.nodes[0].reap_state.is_terminal_for_gating(),
        "and marion has not written it off as orphaned either, which is what a node whose owner \
         died would be: {:?}",
        tree.nodes[0]
    );
}

// ---------------------------------------------------------------------------------------------
// §5.4's handle verbs, against a node that has no contract
// ---------------------------------------------------------------------------------------------

/// **A root's handle resolves — and resolves to the terminal status a root has, not to a contract
/// it cannot write.**
///
/// This is the one place where `wait` is genuinely a different composition for the two MCP
/// surfaces, and the difference is §9's rather than this module's: a child's `wait` ends by reading
/// `contracts/<task_id>.json`, and **a root has no `TaskContract` at all**. `agent/spawn` says so on
/// the wire by answering a root with no `task_id`, which is why `background::Handed` carries the
/// handle and the contract as two fields instead of one.
///
/// Three ways this can be got wrong, and each is asserted against here because each produces a
/// confident, wrong, `isError`-shaped answer rather than a crash:
///
/// 1. **Reading `contracts/<agent-id>.json`** — what a single-field handle would do. Nothing writes
///    that path, so a root that ran perfectly comes back as `NoContract`: marion claiming it lost
///    an answer that never existed.
/// 2. **Reporting the absence as a failure.** A root that exited `Ok` is a success. Routing it
///    through `spawn_result`'s error shape would make every good run read like a broken one.
/// 3. **Answering the second `wait` with "that produced no contract".** True of the file and false
///    of the run — the node did work and its `events.jsonl` is on disk. The three `Collected`
///    variants exist so this sentence can be the root's own.
///
/// `status` is checked in the same test and against the same handle, because §5.4 permits `status`
/// in any state *including terminal* and the handle a `wait` has collected must still name a
/// readable node. It is also the staleness check at this surface: the answer must be the state the
/// supervisor holds now, not the `Spawning` the handle was minted at.
#[test]
fn a_roots_wait_returns_the_terminal_status_it_has_rather_than_a_contract_it_cannot_write() {
    let fx = fixture("mcp-top-root-wait");
    let mut s = fx.server();
    s.call("initialize", json!({}));

    let spawned = s.tool("spawn", spawn_args(true));
    assert_eq!(
        spawned["result"]["isError"],
        json!(false),
        "a backgrounded root spawn is answered: {spawned}"
    );
    let handed = Server::text(&spawned);
    assert!(
        !handed.contains("completed task contract"),
        "the handle must not promise a document §9 gives this node none of: {handed}"
    );
    let task_id = handed
        .split_once("task_id \"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(id, _)| id.to_string())
        .unwrap_or_else(|| panic!("the handle names the task_id a `wait` addresses: {handed}"));

    // Before the node ends: `status` reads the supervisor, and the handle is not collectable yet.
    let early = s.tool("status", json!({"task_id": task_id}));
    assert_eq!(
        early["result"]["isError"],
        json!(false),
        "a `status` on a live root is answered: {early}"
    );
    let early_text = Server::text(&early);
    assert!(
        !early_text.contains("receive its task contract"),
        "`status` must not tell a caller that waiting on a root will produce a contract §9 gives \
         it none of — it is the verb a caller uses to decide whether waiting is worth it: \
         {early_text}"
    );

    until("the root's harness really launched", || {
        std::fs::read_dir(&fx.started).is_ok_and(|d| d.count() > 0)
    });
    fx.open_gate();

    let waited = s.tool("wait", json!({"task_id": task_id}));
    let text = Server::text(&waited);
    // **The status this bed produces is `Failed`, and deliberately not asserted on.** The shim is a
    // plain-text `codex` that never reaches marion's bridge, which marion judges a failed run for
    // its own reasons — nothing to do with roots. What is under test is the *shape* of the answer
    // for a node that has no contract; `bridge::tests::a_roots_result_is_an_error_only_when_the_root
    // _did_not_finish` is where the status-to-`isError` mapping is pinned across all six, including
    // the `Ok` this bed cannot produce.
    assert!(
        text.contains("root"),
        "the answer says what kind of node this was, because that is why there is no contract: \
         {text}"
    );
    assert!(
        text.contains("events.jsonl"),
        "and points at the record that does exist, rather than ending on an absence: {text}"
    );
    assert!(
        !text.contains("cannot hand you its contract") && !text.contains("could not read"),
        "and never reports a successful root as an answer marion lost — which is what reading \
         `contracts/<agent-id>.json` produces: {text}"
    );

    // A second `wait` is refused, in the root's own words.
    let again = s.tool("wait", json!({"task_id": task_id}));
    let again_text = Server::text(&again);
    assert_eq!(
        again["result"]["isError"],
        json!(true),
        "an outcome is delivered once: {again}"
    );
    assert!(
        again_text.contains("no task contract"),
        "and says why there is no second copy of a document, rather than claiming the run \
         produced nothing: {again_text}"
    );
    assert!(
        !again_text.contains("failed before reaching a terminal state"),
        "which is the child sentence and is false about a root that finished: {again_text}"
    );

    // And `status` still resolves the collected handle, with the state the supervisor holds *now*.
    let late = s.tool("status", json!({"task_id": task_id}));
    assert_eq!(
        late["result"]["isError"],
        json!(false),
        "§5.4 permits `status` against a terminal target, and a collected handle still names a \
         node: {late}"
    );
    let late_text = Server::text(&late);
    assert_ne!(
        late_text, early_text,
        "`status` answered the same thing before and after the node ended, so it is being served \
         from the handle rather than read from the supervisor"
    );
    assert!(s.close().success());
}
