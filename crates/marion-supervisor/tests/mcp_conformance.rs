//! **Black-box MCP conformance, spoken to the compiled binary over stdio.**
//!
//! Everything here drives `marion-supervisor mcp` as a child process and asserts on the bytes it
//! writes back. Nothing imports `bridge::parse`, constructs a `Request`, or compares against a
//! hand-built reply literal — the model is `tests/fixtures/s6/mcp-server-frames.jsonl`, which is a
//! recording of frames at exactly this boundary.
//!
//! # Why the boundary and not the function
//!
//! The unit tests inside `bridge.rs` are still worth having, but they cannot see the two things
//! that have actually broken here. Both are properties of the *loop*, not of any function it calls:
//! the ordering between a flush and the readiness marker, and the state carried between one frame
//! and the next. A test that calls `initialize_result` directly cannot observe either, and the
//! version-negotiation defect this suite was written alongside survived four spec revisions behind
//! a unit test that compared the reply to the same constant that produced it.
//!
//! Each test below names the measurement it preserves. Where a fixture recorded the behaviour, the
//! fixture is cited.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

use serde_json::{Value, json};

/// How long a reply may take before the test calls it absent.
///
/// **This bound is what makes "the bridge must answer" a real assertion.** Reading the child's
/// stdout directly would block forever when the answer never comes, and a test that fails only by
/// hanging is not a test — it is a timeout, indistinguishable from a slow machine and useless in
/// a mutation run. This was not hypothetical: with the `Unknown` arm mutated to drop the frame,
/// the naive version of this harness hung instead of failing.
///
/// Generously above anything observed (these round trips are sub-millisecond) so that a loaded CI
/// box cannot make it flake, and far below any watchdog.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// A scratch directory that removes itself however the test leaves.
///
/// **Cleanup on `Drop`, not at the end of the body.** Moving the `remove_dir_all` above the final
/// assertion is not enough: a test can fail at *any* assertion, and the readiness tests below have
/// several before the last one. Under the "unknown method silently dropped" mutation one of them
/// panics on its third line and left `marion-conformance-early-*` behind every run. Unwinding runs
/// destructors, so this is the only placement that holds for a failing test as well as a passing
/// one.
struct Scratch {
    path: std::path::PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "marion-conformance-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn marker(&self) -> std::path::PathBuf {
        self.path.join("ready")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A live bridge process, its stdin, and a channel fed by a thread draining its stdout.
struct Bridge {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
}

impl Bridge {
    /// Spawn the real binary as a node's bridge.
    ///
    /// The three env keys are the declaration marion writes for every node. `MARION_DEPTH` is 1 —
    /// a child, not a root — so `report` is permitted and the s9 policy below is about the child's
    /// outcome rather than about §5.4's root refusal.
    fn spawn() -> Self {
        Self::spawn_with(|_| {})
    }

    fn spawn_with(configure: impl FnOnce(&mut Command)) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_marion-supervisor"));
        cmd.arg("mcp")
            .env("MARION_AGENT_ID", "019f-conformance")
            .env("MARION_AGENT_TYPE", "codex")
            .env("MARION_DEPTH", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        configure(&mut cmd);
        let mut child = cmd.spawn().expect("marion's own bridge starts");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        // The reader runs on its own thread so that a missing reply is a *timeout on the channel*
        // rather than a blocked test. The sender drops at EOF, which is how `finish` learns the
        // stream really ended instead of merely going quiet.
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        Self { child, stdin, rx }
    }

    /// Write one raw line. Raw, so that a test can send something that is not JSON.
    fn send_raw(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn send(&mut self, frame: &Value) {
        self.send_raw(&frame.to_string());
    }

    /// Read the next frame, **failing in bounded time** rather than blocking forever.
    fn recv(&mut self) -> Value {
        let line = match self.rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(l) => l,
            Err(RecvTimeoutError::Timeout) => panic!(
                "the bridge wrote nothing within {REPLY_TIMEOUT:?} where a reply was required — a \
                 frame carrying an id must always be answered, and a client waiting on this id \
                 would wait forever"
            ),
            Err(RecvTimeoutError::Disconnected) => {
                panic!("the bridge closed its stdout without answering")
            }
        };
        serde_json::from_str(&line).expect("every answer is one JSON-RPC frame")
    }

    fn initialize(&mut self, offered: &str) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": offered},
        }));
        self.recv()
    }

    /// Close stdin and collect everything still unread.
    ///
    /// **This is how a "no reply" assertion is made honestly.** A test cannot prove silence by
    /// waiting; it proves it by ending the session and reading the stream to EOF, at which point
    /// what was never written is the empty list.
    fn finish(mut self) -> Vec<Value> {
        drop(self.stdin);
        let _ = self.child.wait();
        // Drain to disconnect. The child has exited and its stdout is closed, so the reader thread
        // has finished and dropped the sender — this terminates rather than waiting on the bound.
        let mut rest = Vec::new();
        loop {
            match self.rx.recv_timeout(REPLY_TIMEOUT) {
                Ok(l) if l.trim().is_empty() => {}
                Ok(l) => rest.push(serde_json::from_str(&l).expect("one JSON-RPC frame")),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    panic!("the bridge exited but its stdout never closed")
                }
            }
        }
        rest
    }
}

// ---- protocol version negotiation ------------------------------------------------------------

/// Every version listed in `SUPPORTED_PROTOCOL_VERSIONS` has a fixture, every fixture names a
/// claimed version, and the **binary** echoes each one.
///
/// This is the review trigger from `bridge::SUPPORTED_PROTOCOL_VERSIONS` made executable in both
/// directions: adding a version without a fixture fails, and leaving a fixture behind for a version
/// that has been dropped fails too. The fixture is not decoration — each file records an offer and
/// the answer a real client would receive, so the pair can be read by a person deciding whether the
/// claim is still honest.
#[test]
fn every_claimed_protocol_version_has_a_fixture_and_is_echoed() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/protocol")
        .canonicalize()
        .expect("tests/fixtures/protocol exists");

    let claimed: Vec<String> = marion_supervisor::bridge::SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .map(|s| s.to_string())
        .collect();

    let mut found: Vec<String> = std::fs::read_dir(&dir)
        .expect("readable")
        .filter_map(|e| {
            let name = e.ok()?.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".jsonl").map(str::to_string)
        })
        .collect();
    found.sort();
    let mut want = claimed.clone();
    want.sort();
    assert_eq!(
        found,
        want,
        "every claimed version needs a fixture under {} and every fixture needs to be claimed — \
         see the review trigger on bridge::SUPPORTED_PROTOCOL_VERSIONS",
        dir.display()
    );

    for version in &claimed {
        let path = dir.join(format!("{version}.jsonl"));
        let body = std::fs::read_to_string(&path).expect("fixture is readable");
        let frames: Vec<Value> = body
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with("//"))
            .map(|l| serde_json::from_str(l).expect("each fixture line is one JSON object"))
            .collect();
        let offer = frames
            .iter()
            .find(|f| f["dir"] == "in")
            .expect("the fixture records the client's frame");
        let expected = frames
            .iter()
            .find(|f| f["dir"] == "out")
            .expect("the fixture records marion's answer");
        assert_eq!(
            offer["frame"]["params"]["protocolVersion"],
            *version,
            "{}: the fixture's own offer must be the version it is named for",
            path.display()
        );

        let mut b = Bridge::spawn();
        let got = b.initialize(version);
        b.finish();
        assert_eq!(
            got["result"]["protocolVersion"],
            *version,
            "{}: a claimed version is echoed, not answered with something else",
            path.display()
        );
        assert_eq!(
            got["result"]["protocolVersion"],
            expected["frame"]["result"]["protocolVersion"],
            "{}: the binary and the fixture disagree",
            path.display()
        );
    }
}

/// **The mutation this kills: answering a constant regardless of the offer.**
///
/// Two offers that marion claims, each of which must come back unchanged. No single constant can
/// satisfy both, so a bridge that ignores the offer fails here whatever constant it picks — which
/// is precisely what the previous unit test could not detect, because it compared the answer to the
/// constant that produced it.
#[test]
fn the_answer_tracks_the_offer_and_no_constant_satisfies_both_rows() {
    let mut a = Bridge::spawn();
    let first = a.initialize("2025-06-18");
    a.finish();

    let mut b = Bridge::spawn();
    let second = b.initialize("2024-11-05");
    b.finish();

    assert_eq!(first["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(second["result"]["protocolVersion"], "2024-11-05");
    assert_ne!(
        first["result"]["protocolVersion"], second["result"]["protocolVersion"],
        "if these ever agree, the bridge has stopped reading the client's offer"
    );
}

/// An offer newer than anything marion claims is **clamped**, never echoed blind.
///
/// Echoing an unknown version would be marion claiming to speak a revision it has never been
/// compiled against — the failure mode `2026-07-28` makes concrete, since that revision deletes the
/// handshake this very frame belongs to.
#[test]
fn an_offer_newer_than_marion_claims_is_clamped_to_what_marion_has() {
    let mut b = Bridge::spawn();
    let v = b.initialize("2026-07-28");
    b.finish();
    let answered = v["result"]["protocolVersion"].as_str().unwrap();
    assert_ne!(
        answered, "2026-07-28",
        "marion does not claim a revision it cannot speak"
    );
    assert!(
        marion_supervisor::bridge::SUPPORTED_PROTOCOL_VERSIONS.contains(&answered),
        "and what it does answer is on its own list: {answered}"
    );
    assert_eq!(
        answered,
        *marion_supervisor::bridge::SUPPORTED_PROTOCOL_VERSIONS
            .last()
            .unwrap(),
        "specifically the ceiling"
    );
}

// ---- the s6 idempotence property ---------------------------------------------------------------

/// **A repeated `initialize` is tolerated** — measured, s6.
///
/// `tests/fixtures/s6/mcp-server-frames.jsonl` caught codex 0.146.0 issuing five `initialize` +
/// `tools/list` sequences across its runs, two of them within a single `exec`. marion-testsupport
/// records 0.146.1/0.147.0 spawning the bridge once instead: *"A bridge that tolerates a second
/// spawn is still correct; one that requires it never was."*
///
/// This is the test that constrains the initialization state added alongside it. The obvious way to
/// implement "reject `tools/call` before `initialize`" is a state machine that also rejects a
/// second `initialize`, and that implementation breaks a real codex.
///
/// **The mutation this kills: rejecting a repeated `initialize`.**
#[test]
fn a_repeated_initialize_is_answered_exactly_like_the_first() {
    let mut b = Bridge::spawn();
    let first = b.initialize("2025-06-18");
    b.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    b.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    let list_one = b.recv();

    // s6's second sequence, identical to the first.
    let second = b.initialize("2025-06-18");
    b.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    b.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    let list_two = b.recv();
    b.finish();

    assert_eq!(
        first, second,
        "s6's second `initialize` gets the same answer as the first, not an error"
    );
    assert!(
        first["error"].is_null(),
        "and neither is an error at all: {first}"
    );
    assert_eq!(
        list_one, list_two,
        "and the tools survive the repeat unchanged"
    );
}

/// A second `initialize` re-negotiates from **its own** offer rather than replaying the first.
///
/// Tolerating a repeat must not mean caching the first answer: a client that reconnects with a
/// different version would be told what the previous connection agreed.
#[test]
fn a_repeated_initialize_renegotiates_from_its_own_offer() {
    let mut b = Bridge::spawn();
    let first = b.initialize("2024-11-05");
    let second = b.initialize("2025-11-25");
    b.finish();
    assert_eq!(first["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(
        second["result"]["protocolVersion"], "2025-11-25",
        "the repeat is negotiated afresh, not replayed from the first"
    );
}

// ---- notifications, unknown methods, and undecodable lines -------------------------------------

/// **Unknown notifications are dropped without a reply** — and the proof is EOF, not a timeout.
///
/// Every frame here is id-less, which is what makes it a notification under JSON-RPC regardless of
/// its name. `notifications/initialized` is s6's; the rest are names marion has never heard of, and
/// one is deliberately outside the `notifications/` namespace to pin that the rule is *the absent
/// id*, not the prefix that used to be matched here.
#[test]
fn unknown_notifications_are_dropped_without_a_reply() {
    let mut b = Bridge::spawn();
    b.initialize("2025-06-18");
    for note in [
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 1}}),
        json!({"jsonrpc": "2.0", "method": "notifications/nothing_marion_knows"}),
        // Not in the `notifications/` namespace at all: still a notification, because no id.
        json!({"jsonrpc": "2.0", "method": "some/other/verb"}),
    ] {
        b.send(&note);
    }
    let unread = b.finish();
    assert!(
        unread.is_empty(),
        "an id-less frame expects no reply, whatever it is called — got {unread:?}"
    );
}

/// **An unknown *method* carrying an id is answered `-32601`, not ignored.**
///
/// The distinction this pins is the one the deleted `starts_with("notifications/")` arm destroyed:
/// `notifications/foo` *with* an id is a request, not a notification, and swallowing it leaves a
/// client waiting on an id forever.
///
/// **The mutation this kills: an unknown method silently dropped.**
#[test]
fn an_unknown_method_with_an_id_is_answered_method_not_found() {
    let mut b = Bridge::spawn();
    b.initialize("2025-06-18");
    for (id, method) in [
        (7, "resources/list"),
        (8, "prompts/list"),
        (9, "completion/complete"),
        // The case the prefix arm used to swallow.
        (10, "notifications/initialized"),
    ] {
        b.send(&json!({"jsonrpc": "2.0", "id": id, "method": method}));
        let v = b.recv();
        assert_eq!(
            v["id"], id,
            "the answer quotes the id that was waiting: {v}"
        );
        assert_eq!(
            v["error"]["code"], -32601,
            "{method} with an id is a request, and an unknown request is method-not-found: {v}"
        );
        assert!(v["result"].is_null(), "an error carries no result: {v}");
    }
    assert!(b.finish().is_empty());
}

/// A line that is not JSON is answered `-32700`, with a null id.
///
/// It used to be dropped by a bare `continue`. The only error code marion ever emitted was `-32601`.
#[test]
fn a_line_that_is_not_json_is_answered_parse_error() {
    let mut b = Bridge::spawn();
    b.initialize("2025-06-18");
    b.send_raw("{this is not json");
    let v = b.recv();
    assert_eq!(
        v["error"]["code"], -32700,
        "unparseable input is -32700: {v}"
    );
    assert!(
        v["id"].is_null(),
        "there is no id to quote, because nothing parsed: {v}"
    );
    // And the session survives it: a parse error is not a disconnect.
    b.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}));
    assert_eq!(
        b.recv()["id"],
        3,
        "the bridge keeps serving after a bad line"
    );
    b.finish();
}

/// JSON that carries no `method` is answered `-32600`; a stray *response* is dropped in silence.
///
/// The silence is the one that is deliberate: marion sends the client no requests, so a `result`
/// frame is an answer to nothing, and answering an answer is how two peers loop forever.
#[test]
fn json_that_is_not_a_request_is_answered_invalid_request_but_a_stray_response_is_not() {
    let mut b = Bridge::spawn();
    b.initialize("2025-06-18");

    b.send(&json!({"jsonrpc": "2.0", "id": 11, "params": {"nothing": true}}));
    let v = b.recv();
    assert_eq!(
        v["error"]["code"], -32600,
        "no method is an invalid request: {v}"
    );
    assert_eq!(
        v["id"], 11,
        "and it quotes the id so the caller is freed: {v}"
    );

    b.send(&json!({"jsonrpc": "2.0", "id": 12, "result": {"whatever": true}}));
    b.send(&json!({"jsonrpc": "2.0", "id": 13, "error": {"code": -1, "message": "x"}}));
    let unread = b.finish();
    assert!(
        unread.is_empty(),
        "a response to a request marion never sent is dropped, not answered — got {unread:?}"
    );
}

// ---- the initialization state ------------------------------------------------------------------

/// `tools/call` and `tools/list` before `initialize` are refused by name rather than served.
#[test]
fn tools_before_initialize_are_refused_rather_than_served() {
    for method in ["tools/list", "tools/call"] {
        let mut b = Bridge::spawn();
        b.send(&json!({
            "jsonrpc": "2.0", "id": 1, "method": method,
            "params": {"name": "report", "arguments": {"narrative": "early"}},
        }));
        let v = b.recv();
        b.finish();
        assert_eq!(
            v["error"]["code"], -32002,
            "{method} before initialize is refused, and the refusal is a sentence: {v}"
        );
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("initialize"),
            "the refusal names what is missing: {v}"
        );
    }
}

// ---- the declared surface ----------------------------------------------------------------------

/// **`tools/list` names are bare, in the pinned order, and contain no harness spelling.**
///
/// §5.4's per-harness-spelling rule: Claude Code renders these as `mcp__marion__spawn`, gemini as
/// `mcp_marion_spawn`, opencode as `marionmcp_spawn` (s13). None of those spellings belongs on the
/// wire, and a prefix leaking into the declaration would be invisible to any test that only checked
/// membership.
#[test]
fn tools_list_names_are_bare_and_in_the_pinned_order() {
    let mut b = Bridge::spawn();
    b.initialize("2025-06-18");
    b.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    let v = b.recv();
    b.finish();

    let names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .expect("tools/list carries an array")
        .iter()
        .map(|t| t["name"].as_str().expect("every tool is named"))
        .collect();
    assert_eq!(
        names,
        vec!["spawn", "wait", "status", "list", "report"],
        "the order is pinned because it is what the model reads first"
    );

    let whole = v.to_string();
    assert!(
        !whole.contains("mcp__marion"),
        "no harness spelling belongs in the declaration: {whole}"
    );
    for n in &names {
        assert!(
            !n.contains("__") && !n.contains("marion"),
            "the name on the wire is bare: {n}"
        );
    }
}

// ---- s9's isError policy -------------------------------------------------------------------------

/// **A refusal is a *successful* JSON-RPC response carrying `isError: true`.**
///
/// This is the s9 policy at the process boundary: the call happened, and its failure is a fact the
/// model must read — not a transport fault. A JSON-RPC `error` here would tell the client the call
/// never occurred, and the model would never see the sentence explaining why.
///
/// `wait` with no `task_id` is used because it is the refusal reachable without a supervisor, a
/// socket, or a child; what is under test is the *shape* of a refusal, not which one it is.
///
/// **The mutation this kills: `isError` derived from a Rust `Result`.** A refusal built by
/// returning `Err` and mapping it to a JSON-RPC error passes nothing here.
#[test]
fn a_refusal_is_a_successful_response_carrying_is_error_true() {
    let mut b = Bridge::spawn();
    b.initialize("2025-06-18");
    b.send(&json!({
        "jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": {"name": "wait", "arguments": {}},
    }));
    let v = b.recv();
    b.finish();

    assert!(
        v["error"].is_null(),
        "a refused tool call is not a JSON-RPC error — the call happened: {v}"
    );
    assert_eq!(
        v["result"]["isError"], true,
        "it reports failure to the model through isError: {v}"
    );
    assert!(
        v["result"]["content"][0]["text"]
            .as_str()
            .expect("a refusal is a sentence")
            .contains("task_id"),
        "and the sentence names what was missing: {v}"
    );
    assert_eq!(v["result"]["content"][0]["type"], "text");
}

/// The other half of the same policy: a successful call is `isError: false`, not an absent flag.
///
/// A missing `isError` is read as `false` by most clients, so the two are easy to conflate — and a
/// bridge that omitted it would still look correct in a client while being unable to express s9's
/// distinction at all.
#[test]
fn a_successful_call_carries_is_error_false_explicitly() {
    let mut b = Bridge::spawn();
    b.initialize("2025-06-18");
    b.send(&json!({
        "jsonrpc": "2.0", "id": 6, "method": "tools/call",
        "params": {"name": "report", "arguments": {"narrative": "a child returning normally"}},
    }));
    let v = b.recv();
    b.finish();

    assert!(v["error"].is_null(), "not a transport error: {v}");
    assert!(
        v["result"].get("isError").is_some(),
        "the flag is present rather than implied: {v}"
    );
    assert_eq!(
        v["result"]["isError"], false,
        "a child's `report` is the verb working (§5.4): {v}"
    );
}

// ---- readiness ordering --------------------------------------------------------------------------

/// **The readiness marker is written only after the `tools/list` reply has been flushed.**
///
/// Measured: Claude Code connects its MCP servers asynchronously and does not hold the first turn
/// for them. Against a canned provider the root otherwise exits 0 in 63 ms with no error anywhere —
/// the first turn goes out before `mcp__marion__spawn` exists, and nothing reports that it did.
///
/// The assertion is an *ordering*, so it is made by reading the reply first and only then looking
/// for the file: at the moment the reply lands the marker may legitimately not exist yet, but once
/// it exists the reply must already have been written. The test therefore proves the half that can
/// actually fail — a marker written *before* the flush is observable as a marker that exists while
/// no reply has been read.
///
/// **The mutation this kills: readiness signalled before the `tools/list` flush.**
#[test]
fn readiness_is_written_only_after_the_tools_list_reply_is_flushed() {
    let scratch = Scratch::new("ready");
    let marker = scratch.marker();

    let m = marker.clone();
    let mut b = Bridge::spawn_with(move |cmd| {
        cmd.env("MARION_READY_FILE", &m);
    });

    // `initialize` alone must not be enough: the marker means "the harness has the tool list".
    b.initialize("2025-06-18");
    assert!(
        !marker.exists(),
        "readiness is not initialize — the tools have not been sent yet"
    );

    b.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    let reply = b.recv();
    assert!(
        !reply["result"]["tools"].as_array().unwrap().is_empty(),
        "the reply that was flushed is the tool list"
    );

    // The reply is in hand, so the flush has happened; the marker may now appear.
    let mut appeared = false;
    for _ in 0..200 {
        if marker.exists() {
            appeared = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    b.finish();
    assert!(
        appeared,
        "the marker must be written once the list has gone out, or the root never starts"
    );
}

/// **The marker cannot appear when the reply never reached the harness.**
///
/// This is the test that pins the *ordering* rather than merely the eventual outcome, and it exists
/// because the obvious version of it does not. A test that reads the `tools/list` reply and then
/// polls for the marker passes whether the marker is written before the flush or after: both
/// orderings are separated by microseconds and both end with the marker present. That test was
/// written first, and a mutation moving `signal_ready()` to before the write **survived it**.
///
/// What discriminates the two is a flush that does not succeed. Here the client closes its end of
/// the pipe before asking for the tool list, so the bridge's write fails with `EPIPE`:
///
/// * signalling **after** a successful flush — the harness was never sent anything, so no marker;
/// * signalling **before** the write — the marker appears for a list that was never delivered.
///
/// That is not a contrived distinction. "Ready" is a claim about the harness having the tools, and
/// the 63 ms silent exit is what happens downstream when the claim is false: Claude Code connects
/// MCP servers asynchronously and does not hold the first turn, so a root told marion is ready
/// before it is sends its first turn into a session where `mcp__marion__spawn` does not exist, and
/// exits 0 with no error anywhere.
///
/// **The mutation this kills: readiness signalled before the `tools/list` flush.**
#[test]
fn readiness_is_not_written_when_the_tools_list_reply_could_not_be_delivered() {
    let scratch = Scratch::new("epipe");
    let marker = scratch.marker();

    let mut child = Command::new(env!("CARGO_BIN_EXE_marion-supervisor"))
        .arg("mcp")
        .env("MARION_AGENT_ID", "019f-conformance")
        .env("MARION_AGENT_TYPE", "codex")
        .env("MARION_DEPTH", "1")
        .env("MARION_READY_FILE", &marker)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("marion's own bridge starts");
    let mut stdin = child.stdin.take().unwrap();
    // Read synchronously and by hand: this test needs to *drop* the read end, which the threaded
    // helper above deliberately never does.
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
               "params": {"protocolVersion": "2025-06-18"}})
    )
    .unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(
        line.contains("protocolVersion"),
        "the bridge is up and initialized: {line}"
    );
    assert!(!marker.exists(), "initialize is not readiness");

    // The harness goes away. Every subsequent write by the bridge fails.
    drop(reader);

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
    )
    .unwrap();
    stdin.flush().unwrap();

    // Long enough that a marker written on the way past would have landed.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let claimed_ready = marker.exists();

    drop(stdin);
    let _ = child.wait();

    assert!(
        !claimed_ready,
        "the bridge announced readiness for a tool list the harness never received — a root now \
         starts its first turn against a session where marion's tools do not exist, and exits 0 \
         with no error anywhere"
    );
}

/// The half of the ordering that a mutation can break: **no marker before the list is asked for.**
///
/// Signalling readiness at startup, or on `initialize`, is the natural wrong implementation — and
/// it is exactly the one that reintroduces the 63 ms silent exit. This waits long enough that an
/// eager marker would have appeared.
#[test]
fn readiness_is_not_written_before_tools_list_is_even_requested() {
    let scratch = Scratch::new("early");
    let marker = scratch.marker();

    let m = marker.clone();
    let mut b = Bridge::spawn_with(move |cmd| {
        cmd.env("MARION_READY_FILE", &m);
    });
    b.initialize("2025-06-18");
    b.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    // A round trip that is *not* tools/list, to prove the bridge is alive and serving.
    b.send(&json!({"jsonrpc": "2.0", "id": 4, "method": "resources/list"}));
    assert_eq!(
        b.recv()["error"]["code"],
        -32601,
        "the bridge is up and answering"
    );

    std::thread::sleep(std::time::Duration::from_millis(250));
    let early = marker.exists();
    b.finish();
    assert!(
        !early,
        "readiness was signalled before the tool list was ever requested: the root's first turn \
         can now go out before marion's tools exist, which is the 63 ms silent exit"
    );
}

// ---- the s6 recording, replayed --------------------------------------------------------------------

/// **s6's captured client frames, replayed against today's binary.**
///
/// The `in` frames are what a real `codex exec` 0.146.0 sent. This asserts marion still answers
/// every one of them with a well-formed frame quoting the right id — the property that made the
/// capture worth keeping.
///
/// It deliberately does **not** assert the recorded `out` frames verbatim: those record marion
/// answering `2024-11-05` to an offer of `2025-06-18`, which is the defect negotiation fixed. The
/// fixture is a historical capture, and `tests/fixtures/s6/README.md` says so.
#[test]
fn the_frames_a_real_codex_sent_are_all_still_answered() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/s6/mcp-server-frames.jsonl");
    let body = std::fs::read_to_string(&path).expect("s6's capture is readable");

    let inbound: Vec<Value> = body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("one JSON object per line"))
        .filter(|f| f["dir"] == "in")
        .map(|f| f["frame"].clone())
        .collect();
    assert!(
        inbound.len() >= 10,
        "s6 recorded a whole session, not a frame or two"
    );

    let mut b = Bridge::spawn();
    let mut answered = 0usize;
    for frame in &inbound {
        b.send(frame);
        if frame.get("id").is_some() {
            let v = b.recv();
            assert_eq!(
                v["id"], frame["id"],
                "every answer quotes its request's id: {v}"
            );
            assert!(
                v["error"].is_null(),
                "no frame a real codex sent is refused: {frame} -> {v}"
            );
            answered += 1;
        }
    }
    let unread = b.finish();
    assert!(
        unread.is_empty(),
        "and nothing extra was written — the id-less frames got no reply: {unread:?}"
    );
    assert_eq!(
        answered,
        inbound.iter().filter(|f| f.get("id").is_some()).count(),
        "every request in the capture was answered exactly once"
    );
}

/// The `agent_type` a `spawn` may name, read off `tools/list` — for the one node that reads it.
fn spawn_agent_type_description(listed: &Value) -> String {
    let tools = listed["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list answers with a list: {listed}"));
    let spawn = tools
        .iter()
        .find(|t| t["name"] == "spawn")
        .unwrap_or_else(|| panic!("spawn is declared: {listed}"));
    assert!(
        spawn["inputSchema"]["properties"]["agent_type"]["enum"].is_null(),
        "no enum: `acp:<command>` must stay legal"
    );
    spawn["inputSchema"]["properties"]["agent_type"]["description"]
        .as_str()
        .unwrap_or_else(|| panic!("agent_type carries a description: {spawn}"))
        .to_string()
}

/// A bridge for a tree whose `.marion/agents.toml` is `file`, asked for `tools/list` once.
fn tools_list_for_tree(tag: &str, file: &str) -> Value {
    let dir = Scratch::new(tag);
    let path = dir.path.join(".marion/agents.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, file).unwrap();
    let mut b = Bridge::spawn_with(|cmd| {
        cmd.env("MARION_REPO", &dir.path);
    });
    b.initialize("2025-06-18");
    b.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    b.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    let listed = b.recv();
    b.finish();
    listed
}

/// **A node reads the tree's own agent types off `tools/list`.** The bridge knows its tree
/// through `MARION_REPO`, so a `[[agent]]` row in that tree's `.marion/agents.toml` is named — with
/// its description — in the `spawn` schema, beside every built-in, per request rather than once
/// at startup: the file is the operator's and may change under a running node.
#[test]
fn tools_list_names_the_trees_user_defined_agent_types() {
    let listed = tools_list_for_tree(
        "agent-types",
        "[[agent]]\nname = \"reviewer\"\nharness = \"codex\"\n\
         description = \"Reviews a diff and reports findings; never edits.\"\n",
    );
    let description = spawn_agent_type_description(&listed);
    assert!(
        description.contains("reviewer (Reviews a diff and reports findings; never edits.)"),
        "the row and its description are offered to the node: {description}"
    );
    assert!(
        description.contains("codex-impl") && description.contains("claude"),
        "beside the built-ins: {description}"
    );
}

/// A file the bridge cannot use is **said**, in the same place, and the built-ins are still
/// offered: the node learns why its `reviewer` is missing instead of being told a shorter list.
#[test]
fn tools_list_carries_the_agent_types_files_own_refusal() {
    let listed = tools_list_for_tree(
        "agent-types-broken",
        "[[agent]]\nname = \"reviewer\"\nharness = \"codex\"\ndescription = \"r\"\n\
         [[agent]]\nname = \"reviewer\"\nharness = \"gemini\"\ndescription = \"r\"\n",
    );
    let description = spawn_agent_type_description(&listed);
    assert!(
        description.contains("agents.toml") && description.contains("defined twice"),
        "the file's own refusal, verbatim: {description}"
    );
    assert!(
        !description.contains("reviewer ("),
        "a row from a refused file is not offered: {description}"
    );
    assert!(description.contains("codex-impl"), "{description}");
}
