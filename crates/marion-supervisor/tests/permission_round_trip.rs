//! S9 — the **inbound** half of Claude Code's control channel, measured (design §11 item 14).
//!
//! Until this test existed, `can_use_tool` was designed on decompilation: `tests/fixtures/s1/`
//! holds **zero** inbound `control_request` frames, and the branch in
//! [`marion_supervisor::root::launch`] that answers one had never run. §11 item 14: *"The demux map
//! and the entire permission path rest on it."*
//!
//! # How a permission ask is provoked without a model
//!
//! `report` is deliberately **absent** from [`root::ROOT_ALLOWED_TOOLS`] — §9 rejects `report` on a
//! node with no contract, and a root has none. So pointing the canned root at
//! `mcp__marion__report` gives the exact situation `root.rs` warns about ("omitting a *reachable*
//! verb would deny calls that then block until the root's bound expires") with **no argv surgery**:
//! marion's own production invocation, its own bridge, its own allowlist. The CLI offers the tool
//! (MCP tools are the availability axis) and refuses to run it unasked (the permission axis), so it
//! asks — over stdout, as a `control_request`.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test permission_round_trip
//! ```
//!
//! Needs real `claude` (2.1.220) on `PATH`. No `codex` child is involved and no model is called.
//! Set `MARION_S9_RECORD=1` to rewrite `tests/fixtures/s9/` from the run instead of only asserting
//! against it — that is how the committed fixture was produced, and it is why the fixture is a
//! recording rather than something typed out by hand.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use marion_provider::script::ROOT_TOOL_USE_ID;
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::root::{
    self, RootSpec, can_use_tool_request, deny_response, initialize_request,
    is_control_response_to, user_message,
};
use serde_json::{Value, json};

/// The verb the canned root reaches for. Real, served by marion's own bridge, and **not** in
/// `ROOT_ALLOWED_TOOLS`.
const ASKED_TOOL: &str = "mcp__marion__report";

/// The root's per-episode `Blocked` budget for the expiry test. Short enough to keep the suite
/// fast, long enough that "the bound was actually waited out" is measurable.
const BLOCKED_BOUND: Duration = Duration::from_millis(750);

const MCP_READY: Duration = Duration::from_secs(60);

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/s9")
}

fn recording() -> bool {
    std::env::var_os("MARION_S9_RECORD").is_some()
}

fn on_path(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn scratch(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("marion-s9-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    p.canonicalize().expect("scratch dir canonicalises")
}

/// What the canned root reaches for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    /// marion's own `report` verb: real, served by marion's own bridge, and absent from
    /// `ROOT_ALLOWED_TOOLS`. **No argv surgery** — this is marion's production invocation.
    MarionReport,
    /// A built-in `Bash` call touching a path outside the root's cwd. Needs one argv edit
    /// (`--tools ""` → `--tools Bash`), since marion's own root is compiled with no built-ins at
    /// all. Recorded only to show that the ask's field set is **not** fixed across tool kinds.
    BuiltinBash,
}

/// Everything one probe needs: a canned provider aimed at the non-allowlisted verb, and a prepared
/// root node built by marion's own [`root::prepare`].
struct Fixture {
    root_dir: PathBuf,
    server: CannedServer,
    node: root::RootNode,
}

fn prepare(name: &str, target: Target) -> Fixture {
    let root_dir = scratch(name);
    let repo = root_dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let state = root_dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    // Outside the root's cwd, so the CLI must ask. Denied in the test, so it never runs.
    let outside = root_dir.join("outside/marion-s9.txt");
    let script = match target {
        Target::MarionReport => Script {
            root_tool: ASKED_TOOL.to_string(),
            root_tool_input: json!({"narrative": "s9 probe: a verb the root may not use"}),
            ..Script::default()
        },
        Target::BuiltinBash => Script {
            root_tool: "Bash".to_string(),
            root_tool_input: json!({
                "command": format!("touch {}", outside.display()),
                "description": "s9 probe",
            }),
            ..Script::default()
        },
    };

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: root_dir.join("provider-requests.jsonl"),
        script: Script {
            root_final_text: "s9: the turn continued after the permission was answered."
                .to_string(),
            ..script
        },
    })
    .expect("the canned provider binds");

    let mut node = root::prepare(&RootSpec {
        agent_type: "claude".into(),
        // The turn every probe in this file drives. It is compiled into the node rather than
        // handed to `launch`, so the prompt that was prepared and the prompt that is written are
        // one string.
        prompt: "Call the report tool.".into(),
        repo: repo.canonicalize().unwrap(),
        state,
        base_url: server.base_url(),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        model: None,
    })
    .expect("the root node prepares");

    if target == Target::BuiltinBash {
        // The availability axis, not the permission one (§5.2). marion sets it empty; this probe
        // is the one place in the suite that does not, and it is deliberately explicit about it.
        let i = node
            .invocation
            .args
            .iter()
            .position(|a| a == "--tools")
            .expect("compile_headless always emits the availability axis");
        node.invocation.args[i + 1] = "Bash".into();
    }

    Fixture {
        root_dir,
        server,
        node,
    }
}

fn wait_for_ready(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    path.exists()
}

/// What marion answers a `can_use_tool` with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Answer {
    /// §9's rule: nobody to ask, so the bound expires and the request is denied.
    Deny,
    /// The other branch of the same wire, so the fixture records both.
    Allow,
}

/// Both directions of one run, verbatim.
struct Capture {
    stdin: Vec<String>,
    stdout: Vec<String>,
    exit_code: Option<i32>,
    /// Kept only so a run that never got as far as asking says why.
    stderr: String,
}

impl Capture {
    fn frames(&self) -> Vec<Value> {
        self.stdout
            .iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    fn ask(&self) -> Value {
        self.frames()
            .into_iter()
            .find(|f| can_use_tool_request(f).is_some())
            .unwrap_or_else(|| {
                panic!(
                    "no inbound can_use_tool frame: the CLI must ask for a tool that is not in \
                     --allowedTools.\nstderr:\n{}\nframes:\n{}",
                    self.stderr,
                    self.stdout.join("\n")
                )
            })
    }

    fn result(&self) -> Value {
        self.frames()
            .into_iter()
            .find(|f| f["type"] == "result")
            .expect("the turn must have reached its terminal result frame")
    }

    /// The `tool_result` the CLI produced for the asked-about call.
    fn tool_result(&self) -> Value {
        self.frames()
            .into_iter()
            .flat_map(|f| {
                f.pointer("/message/content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            })
            .find(|b| b["type"] == "tool_result" && b["tool_use_id"] == ROOT_TOOL_USE_ID)
            .expect("the call the permission was about must have produced a tool_result")
    }
}

/// Drive one full `stream-json` conversation, answering the permission ask with `answer`, and
/// record both directions.
///
/// This is deliberately *not* [`root::launch`]: launch owns the child's stdin, so it cannot hand
/// back what marion wrote, and it only ever denies. The deny line it would have written is the one
/// written here — [`deny_response`] is called, not imitated.
fn drive(fx: &Fixture, answer: Answer) -> Capture {
    let inv = &fx.node.invocation;
    let mut child = Command::new(&inv.program)
        .args(&inv.args)
        .envs(inv.env.iter().cloned())
        .current_dir(&inv.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("claude starts");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stderr_pipe = child.stderr.take().unwrap();
    let stderr_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr_pipe.read_to_string(&mut s);
        s
    });
    let mut lines = BufReader::new(stdout).lines();

    let mut cap = Capture {
        stdin: vec![],
        stdout: vec![],
        exit_code: None,
        stderr: String::new(),
    };

    assert!(
        wait_for_ready(
            fx.node
                .ready_file
                .as_ref()
                .expect("a duplex root has a readiness marker to gate on"),
            MCP_READY
        ),
        "the bridge never answered tools/list, so mcp__marion__report was never offered"
    );

    let init_id = format!("marion-init-{}", fx.node.agent_id.0);
    let init = initialize_request(&init_id);
    writeln!(stdin, "{init}").unwrap();
    stdin.flush().unwrap();
    cap.stdin.push(init);
    for line in lines.by_ref() {
        let line = line.unwrap();
        let frame: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        cap.stdout.push(line);
        if is_control_response_to(&frame, &init_id) {
            break;
        }
    }

    let turn = user_message("Use the tool you have been given.");
    writeln!(stdin, "{turn}").unwrap();
    stdin.flush().unwrap();
    cap.stdin.push(turn);

    for line in lines.by_ref() {
        let line = line.unwrap();
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        cap.stdout.push(line);
        if let Some((request_id, _tool)) = can_use_tool_request(&frame) {
            let reply = match answer {
                // marion's own production string, byte for byte.
                Answer::Deny => deny_response(
                    &request_id,
                    "marion: no permission answerer in M1; the root's Blocked bound expired",
                ),
                // `updatedInput` is optional — S9 measured a bare `allow` running the tool with the
                // model's original input — but marion echoes the input back rather than relying on
                // that, because an answerer that rewrites arguments is the point of the field.
                Answer::Allow => json!({
                    "type": "control_response",
                    "response": {
                        "subtype": "success",
                        "request_id": request_id,
                        "response": {
                            "behavior": "allow",
                            "updatedInput": frame.pointer("/request/input")
                                .cloned().unwrap_or_else(|| json!({})),
                        },
                    },
                })
                .to_string(),
            };
            writeln!(stdin, "{reply}").unwrap();
            stdin.flush().unwrap();
            cap.stdin.push(reply);
        }
        if frame["type"] == "result" {
            break;
        }
    }

    drop(stdin);
    cap.exit_code = child.wait().expect("claude exits").code();
    cap.stderr = stderr_thread.join().unwrap_or_default();
    cap
}

// ---------------------------------------------------------------------------------------------
// Redaction, matching the S6/S7 house style.
// ---------------------------------------------------------------------------------------------

/// UUIDs become `<UUID-1>`, `<UUID-2>`, … **numbered by first appearance**.
///
/// S6 flattened every uuid to `<UUID>`. That would destroy the one correlation this fixture exists
/// to record — that the `control_response` names the same `request_id` the `control_request` did —
/// so the numbering is a deliberate deviation, not an oversight.
struct Redactor {
    home: String,
    scratch: Vec<String>,
    agent_id: String,
    uuids: Vec<String>,
}

fn uuids_in(s: &str) -> Vec<String> {
    /// `8-4-4-4-12`, as bytes: a uuid is pure ASCII, so byte indices are safe *inside* a match.
    fn is_uuid(w: &[u8]) -> bool {
        let mut at = 0;
        for (n, len) in [8usize, 4, 4, 4, 12].iter().enumerate() {
            if n > 0 {
                if w[at] != b'-' {
                    return false;
                }
                at += 1;
            }
            if !w[at..at + len].iter().all(u8::is_ascii_hexdigit) {
                return false;
            }
            at += len;
        }
        true
    }
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 36 <= b.len() {
        if is_uuid(&b[i..i + 36]) {
            out.push(String::from_utf8_lossy(&b[i..i + 36]).into_owned());
            i += 36;
        } else {
            i += 1;
        }
    }
    out
}

impl Redactor {
    fn new(fx: &Fixture) -> Self {
        let root = fx.root_dir.to_string_lossy().into_owned();
        Self {
            home: std::env::var("HOME").unwrap_or_default(),
            // The same directory appears with and without macOS's `/private` prefix.
            scratch: vec![
                format!("/private{root}"),
                root.clone(),
                root.trim_start_matches("/private").to_string(),
            ],
            agent_id: fx.node.agent_id.0.clone(),
            uuids: Vec::new(),
        }
    }

    fn line(&mut self, s: &str) -> String {
        let mut out = s.to_string();
        for p in &self.scratch {
            if !p.is_empty() {
                out = out.replace(p.as_str(), "<SCRATCH>");
            }
        }
        if !self.home.is_empty() {
            out = out.replace(&self.home, "<HOME>");
        }
        out = out.replace(&self.agent_id, "<AGENT-ID>");
        for u in uuids_in(&out) {
            if !self.uuids.contains(&u) {
                self.uuids.push(u.clone());
            }
            let n = self.uuids.iter().position(|x| *x == u).unwrap() + 1;
            out = out.replace(&u, &format!("<UUID-{n}>"));
        }
        // Two frames carry the operator's whole slash-command, skill, agent, plugin and model
        // catalogue: `system/init`, and — measured here for the first time — the `control_response`
        // to `initialize`, which is ~30 kB of it. Reduced rather than merely scrubbed, exactly as
        // S6 reduced the provider request log: none of it is evidence about this channel, and all
        // of it is machine-specific. The keys are kept so the *shape* still reads.
        const MACHINE_SPECIFIC: [&str; 11] = [
            "slash_commands",
            "skills",
            "agents",
            "plugins",
            "memory_paths",
            "commands",
            "models",
            "available_output_styles",
            "account",
            "pid",
            "capabilities",
        ];
        if let Ok(mut v) = serde_json::from_str::<Value>(&out) {
            let mut reduced = false;
            for at in ["", "/response/response"] {
                let Some(obj) = (if at.is_empty() {
                    Some(&mut v)
                } else {
                    v.pointer_mut(at)
                }) else {
                    continue;
                };
                // Only the two catalogue-bearing frames: `capabilities` on the `system/init` frame
                // is a short, meaningful list and stays.
                if obj["subtype"] != "init" && obj.get("commands").is_none() {
                    continue;
                }
                for k in MACHINE_SPECIFIC {
                    if obj.get(k).is_some() && !(at.is_empty() && k == "capabilities") {
                        obj[k] = json!("<REDACTED-machine-specific>");
                        reduced = true;
                    }
                }
            }
            if reduced {
                out = v.to_string();
            }
        }
        out
    }
}

fn record(fx: &Fixture, cap: &Capture, stem: &str) {
    if !recording() {
        return;
    }
    let dir = fixture_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let mut r = Redactor::new(fx);
    // stdout first, so the numbering of `<UUID-n>` follows the CLI's frames rather than ours.
    for (name, lines) in [("stdout", &cap.stdout), ("stdin", &cap.stdin)] {
        let body: String = lines
            .iter()
            .map(|l| r.line(l) + "\n")
            .collect::<Vec<_>>()
            .concat();
        std::fs::write(dir.join(format!("{stem}.{name}.jsonl")), body).unwrap();
    }
}

fn fixture_lines(name: &str) -> Vec<String> {
    let p = fixture_dir().join(name);
    let s = std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("the committed S9 fixture {} is missing: {e}", p.display()));
    s.lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

fn fixture_frames(name: &str) -> Vec<Value> {
    fixture_lines(name)
        .iter()
        .map(|l| serde_json::from_str(l).expect("every fixture line is one JSON frame"))
        .collect()
}

/// Frames compare equal once the values redaction replaced are ignored: the *shape* is the
/// evidence, and the ids differ per run by construction.
fn shape(v: &Value) -> Value {
    fn scrub(v: &mut Value) {
        match v {
            Value::Object(m) => {
                for (k, val) in m.iter_mut() {
                    if matches!(
                        k.as_str(),
                        "request_id"
                            | "session_id"
                            | "uuid"
                            | "timestamp"
                            | "duration_ms"
                            | "duration_api_ms"
                            | "ttft_ms"
                            | "ttft_stream_ms"
                            | "time_to_request_ms"
                            | "cwd"
                    ) {
                        *val = json!("<scrubbed>");
                    } else {
                        scrub(val);
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(scrub),
            _ => {}
        }
    }
    let mut v = v.clone();
    scrub(&mut v);
    v
}

/// The committed recording is the contract. If `claude` changes the ask, this fails — which is the
/// whole point of committing it, since marion's permission path is keyed on the shape and a silent
/// change to it would have no other symptom.
fn assert_committed_recording_still_matches(fx: &Fixture, cap: &Capture, stdout_fixture: &str) {
    let committed = fixture_frames(stdout_fixture)
        .into_iter()
        .find(|f| can_use_tool_request(f).is_some())
        .expect("the committed recording holds the ask");
    // Redacted the same way the recording was, or the per-run scratch path in `blocked_path` would
    // read as a protocol change.
    let live: Value =
        serde_json::from_str(&Redactor::new(fx).line(&cap.ask().to_string())).unwrap();
    assert_eq!(
        shape(&live),
        shape(&committed),
        "claude 2.1.220's can_use_tool frame no longer matches tests/fixtures/s9/{stdout_fixture}. \
         Re-read root.rs's permission path before re-recording with MARION_S9_RECORD=1."
    );
}

fn survivors(needle: &str) -> Vec<String> {
    Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| l.contains(needle))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn require_claude() {
    assert!(
        on_path("claude"),
        "S9 is about a REAL claude control channel; put `claude` (2.1.220) on PATH"
    );
}

// ---------------------------------------------------------------------------------------------
// The measurements.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_non_allowlisted_verb_makes_the_cli_ask_over_the_control_channel_and_a_denial_lets_it_finish() {
    require_claude();
    let fx = prepare("deny", Target::MarionReport);
    let cap = drive(&fx, Answer::Deny);
    record(&fx, &cap, "can-use-tool-deny");

    let ask = cap.ask();
    assert_eq!(
        ask["request"]["tool_name"], ASKED_TOOL,
        "the ask must name the verb that was refused, not the session"
    );
    assert_eq!(
        ask["request"]["tool_use_id"], ROOT_TOOL_USE_ID,
        "the ask is tied to the model's own tool_use block, which is how marion knows which call \
         it is answering for"
    );
    assert!(
        ask["request_id"].as_str().is_some_and(|s| !s.is_empty()),
        "§5.2: the top-level request_id is load-bearing — a frame without one could be neither \
         answered nor cancelled"
    );

    // The root proceeded. It was not killed, and the denial is a tool error, not a run error.
    let tr = cap.tool_result();
    assert_eq!(tr["is_error"], true, "a denial surfaces as an error result");
    let result = cap.result();
    assert_eq!(result["is_error"], false);
    assert_eq!(result["subtype"], "success");
    assert_eq!(
        result["terminal_reason"], "completed",
        "§9: expiry denies the pending request and lets the root proceed — it does not kill it"
    );
    assert_eq!(
        result["permission_denials"][0]["tool_name"], ASKED_TOOL,
        "the CLI's own terminal frame corroborates that the denial is what happened"
    );
    assert_eq!(cap.exit_code, Some(0));

    assert_committed_recording_still_matches(&fx, &cap, "can-use-tool-deny.stdout.jsonl");

    drop(fx.server);
    let _ = std::fs::remove_dir_all(&fx.root_dir);
}

#[test]
fn an_allowed_permission_actually_runs_the_tool_and_the_answer_reaches_marions_own_bridge() {
    require_claude();
    let fx = prepare("allow", Target::MarionReport);
    let cap = drive(&fx, Answer::Allow);
    record(&fx, &cap, "can-use-tool-allow");

    assert_eq!(cap.ask()["request"]["tool_name"], ASKED_TOOL);
    let tr = cap.tool_result();
    assert_ne!(
        tr["is_error"], true,
        "an allowed call must not surface as an error"
    );
    assert!(
        tr.to_string().contains("report recorded"),
        "the tool really ran: `report recorded` is what marion's own bridge returns, so the \
         allow answer reached the MCP server and not merely the CLI: {tr}"
    );
    let result = cap.result();
    assert_eq!(result["terminal_reason"], "completed");
    assert_eq!(
        result["permission_denials"].as_array().map(Vec::len),
        Some(0),
        "nothing was denied on the allow run"
    );
    assert_eq!(cap.exit_code, Some(0));

    assert_committed_recording_still_matches(&fx, &cap, "can-use-tool-allow.stdout.jsonl");

    drop(fx.server);
    let _ = std::fs::remove_dir_all(&fx.root_dir);
}

#[test]
fn the_supervisors_blocked_bound_expires_into_a_deny_and_the_root_survives_it() {
    // The branch in `root::launch` that §11 item 14 calls out as written-but-unexercised. Every
    // verb the M1 hop reaches is allowlisted, so nothing else in this suite runs it.
    require_claude();
    let fx = prepare("bound", Target::MarionReport);

    let started = Instant::now();
    let outcome = root::launch(&fx.node, BLOCKED_BOUND, MCP_READY).expect("the root runs");
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.denied_permissions,
        vec![ASKED_TOOL.to_string()],
        "the supervisor's own record of what it denied"
    );
    assert!(
        elapsed >= BLOCKED_BOUND,
        "the answer must not have been sent before the bound expired: {elapsed:?}"
    );
    assert_eq!(
        outcome.exit_code,
        Some(0),
        "§9: expired and terminated are different events for a root.\nstderr:\n{}",
        outcome.stderr
    );
    let result = outcome
        .transcript
        .iter()
        .find(|f| f["type"] == "result")
        .expect("the root reached its terminal frame");
    assert_eq!(result["terminal_reason"], "completed");
    assert!(
        outcome
            .transcript
            .iter()
            .any(|f| can_use_tool_request(f).is_some()),
        "the transcript must carry the inbound control_request the supervisor answered"
    );

    drop(fx.server);
    let leaked = survivors(&fx.root_dir.to_string_lossy());
    assert!(
        leaked.is_empty(),
        "leaked processes:\n{}",
        leaked.join("\n")
    );
    let _ = std::fs::remove_dir_all(&fx.root_dir);
}

#[test]
fn the_same_subtype_carries_a_different_field_set_for_a_builtin_tool() {
    // The field set is not fixed. This run is the counter-example that keeps `can_use_tool_request`
    // honest: a parser that read `blocked_path` or `description` would work against marion's own
    // MCP verbs and fail here, and the reverse would fail there.
    require_claude();
    let fx = prepare("builtin", Target::BuiltinBash);
    let cap = drive(&fx, Answer::Deny);
    record(&fx, &cap, "can-use-tool-builtin-deny");

    let ask = cap.ask();
    assert_eq!(ask["request"]["tool_name"], "Bash");
    assert!(
        !ask["request"]["blocked_path"].is_null(),
        "a built-in ask names the path that provoked it; an MCP ask has no such field: {ask}"
    );
    assert!(!ask["request"]["description"].is_null());
    assert!(
        ask["request"]["permission_suggestions"]
            .as_array()
            .is_some_and(|a| a.len() > 1),
        "a built-in ask suggests rules, directories and a mode; an MCP ask suggests one rule"
    );
    assert_eq!(cap.result()["terminal_reason"], "completed");
    assert!(
        !fx.root_dir.join("outside/marion-s9.txt").exists(),
        "the denied command must not have run"
    );

    assert_committed_recording_still_matches(&fx, &cap, "can-use-tool-builtin-deny.stdout.jsonl");

    drop(fx.server);
    let _ = std::fs::remove_dir_all(&fx.root_dir);
}

// ---------------------------------------------------------------------------------------------
// The committed recording is what the parser is held to.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_recorded_response_is_the_string_the_supervisor_actually_writes() {
    // Not "a deny-shaped frame": the byte string `root::deny_response` produces, modulo the
    // request_id redaction. If the two ever diverge, the fixture is no longer evidence about
    // marion.
    let recorded: Value = fixture_lines("can-use-tool-deny.stdin.jsonl")
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.pointer("/response/response/behavior").is_some())
        .expect("the committed recording holds marion's answer");
    let rid = recorded["response"]["request_id"].as_str().unwrap();
    let produced: Value = serde_json::from_str(&deny_response(
        rid,
        "marion: no permission answerer in M1; the root's Blocked bound expired",
    ))
    .unwrap();
    assert_eq!(produced, recorded);
}

#[test]
fn the_ask_carries_the_fields_the_permission_path_reads_and_the_ones_it_may_not_assume() {
    // Verbatim from tests/fixtures/s9/can-use-tool-deny.stdout.jsonl — an **MCP verb** of marion's
    // own. Pasted rather than loaded so the shape is readable at the point it is asserted, and
    // checked against the file below so the paste cannot drift from the recording.
    const MCP_ASK: &str = r#"{"type":"control_request","request_id":"<UUID-4>","request":{"subtype":"can_use_tool","tool_name":"mcp__marion__report","display_name":"Report","input":{"narrative":"s9 probe: a verb the root may not use"},"permission_suggestions":[{"type":"addRules","rules":[{"toolName":"mcp__marion__report"}],"behavior":"allow","destination":"localSettings"}],"tool_use_id":"toolu_marion_spawn_1"}}"#;
    // Verbatim from tests/fixtures/s9/can-use-tool-builtin-deny.stdout.jsonl — the same subtype for
    // a **built-in** tool. `description` and `blocked_path` appear here and are absent above, and
    // `permission_suggestions` carries three entries rather than one. A parser that required
    // either field would work against marion's verbs and fail against Bash.
    const BUILTIN_ASK: &str = r#"{"type":"control_request","request_id":"<UUID-4>","request":{"subtype":"can_use_tool","tool_name":"Bash","display_name":"Bash","input":{"command":"touch <SCRATCH>/outside/marion-s9.txt","description":"s9 probe"},"description":"s9 probe","permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"touch <SCRATCH>/outside/marion-s9.txt"}],"behavior":"allow","destination":"localSettings"},{"type":"addDirectories","directories":["<SCRATCH>/outside"],"destination":"session"},{"type":"setMode","mode":"acceptEdits","destination":"session"}],"blocked_path":"<SCRATCH>/outside/marion-s9.txt","tool_use_id":"toolu_marion_spawn_1"}}"#;

    for (pasted, file) in [
        (MCP_ASK, "can-use-tool-deny.stdout.jsonl"),
        (BUILTIN_ASK, "can-use-tool-builtin-deny.stdout.jsonl"),
    ] {
        assert!(
            fixture_lines(file).iter().any(|l| l == pasted),
            "the line pasted into this test is not in tests/fixtures/s9/{file}; a paste that \
             drifts from the recording is evidence about nothing"
        );
    }

    let mcp_ask: Value = serde_json::from_str(MCP_ASK).unwrap();
    let builtin_ask: Value = serde_json::from_str(BUILTIN_ASK).unwrap();
    assert_eq!(
        can_use_tool_request(&mcp_ask),
        Some(("<UUID-4>".to_string(), "mcp__marion__report".to_string()))
    );
    assert_eq!(
        can_use_tool_request(&builtin_ask),
        Some(("<UUID-4>".to_string(), "Bash".to_string())),
        "only request_id, subtype and tool_name are common to both field sets, and those are \
         exactly the three the supervisor reads"
    );
    assert!(mcp_ask["request"]["blocked_path"].is_null());
    assert!(!builtin_ask["request"]["blocked_path"].is_null());

    // The top-level `request_id` §5.2 calls load-bearing: a request that carried it only inside
    // `request` could be neither answered nor cancelled.
    for ask in [&mcp_ask, &builtin_ask] {
        assert!(ask["request_id"].is_string());
        assert!(ask["request"]["request_id"].is_null());
    }
}
