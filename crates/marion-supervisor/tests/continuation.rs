//! **Turn delivery by continuation, end to end: a message for a `LaunchOnly` node relaunches the
//! same node under its own session, with the message as the prompt.**
//!
//! A `codex exec` child takes one turn from argv and exits; it has no channel for a second one. Its
//! row's `TurnDelivery::Continuation` says how the next turn reaches it instead: the row's resume
//! spelling (`exec resume <thread>`) with the message rendered as the prompt, as a new generation
//! of the same node. These runs drive a **real** codex through marion's canned provider, so the
//! relaunch is a real harness resuming its own thread:
//!
//! 1. **A steer during a §7.6 hold resumes the node.** The child backgrounds a grandchild and
//!    stops unreported, so marion holds it in `Blocked(Descendants)`. The hold is the node's turn
//!    boundary: an operator's `node/steer` there ends it, and the child comes back as generation
//!    two — a second `Spawned`, `exec resume <thread>` carrying the rendered message on argv, and a
//!    provider request that replays the first turn's calls beside the message, delivered
//!    `continuation:gen2`. Generation two reports over the still-live grandchild, and the
//!    contract carries that report, `reported_early`.
//! 2. **A child's end during the hold resumes its parent**, which reads it and reports.
//! 3. **No observed session, no continuation**: the message is dropped, naming why.
//! 4. **The wall clock is the node's, not the generation's.** A wedged generation two is killed
//!    when the *original* bound runs out, so the node ends `TimedOut` inside its one bound.
//!
//! 5. **A `LaunchOnly` root is continued the same way**: a `marion run codex` root that
//!    backgrounds a codex child is relaunched with the child's end as its second generation.
//!
//! The bed is `descendant_gate.rs`'s: a detached supervisor, a `codex` shim ahead of the real
//! binary on its `PATH`, the root and grandchild blocked on gate files, and every wait a fact in
//! the journal.
//!
//! ```sh
//! cargo test -p marion-supervisor --test continuation
//! ```
//!
//! It needs a real `codex` on `PATH`. Every model call is the canned server's: no paid tokens.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_core::node::{BlockReason, NodeState};
use marion_core::paths::ProjectDir;
use marion_core::proto::params::{AgentSpawnParams, NodeSteerParams};
use marion_core::proto::{Call, Method, MethodResult, SpawnCaller};
use marion_core::registry::Replay;
use marion_provider::reqlog::RequestLog;
use marion_provider::{CannedServer, Config, NodeScript, Script, ScriptedCall};
use marion_supervisor::journal::read_path;
use marion_supervisor::socket::project_root;
use marion_testsupport::{
    fixture_repo, kill_hard, on_path, persisted_contracts, pinned_version, scratch, survivors,
};
use serde_json::{Value, json};

mod common;

/// In the child's first prompt only: the shim routes on it to the real binary, and the provider
/// dispatches the child's first turn on it.
const DELEGATOR_MARKER: &str = "MARION-CONTINUATION-DELEGATOR-81c3";
/// In the root's prompt only.
const ROOT_MARKER: &str = "MARION-CONTINUATION-ROOT-81c3";
/// In the steer only — so a provider request carrying it was made by generation two, and the shim
/// routes the relaunch (whose argv carries the message, not the first prompt) to the real binary.
const STEER_MARKER: &str = "MARION-CONTINUATION-STEER-81c3";
/// In the wedging steer only: the shim sleeps on it instead of running codex.
const WEDGE_MARKER: &str = "MARION-CONTINUATION-WEDGE-81c3";
/// In the rendered announcement of a backgrounded child's end (`inbox::child_ended_text`) and in
/// nothing a first generation sends — so the shim routes a relaunch carrying it to the real
/// binary, and the provider answers the turn that carries it.
const PUSH_MARKER: &str = "you backgrounded as task_id";
/// In a child the shim fakes: it waits on its own gate, prints no frame naming a session, and
/// exits — a codex whose stream never named its thread.
const SESSIONLESS_MARKER: &str = "MARION-CONTINUATION-SESSIONLESS-81c3";

const GRANDCHILD_PROMPT: &str = "continuation grandchild: hold until released";
const CHILD_CALL_PREFIX: &str = "call_continuation_child";
const STEER_CALL_PREFIX: &str = "call_continuation_steer";
/// What generation two reports.
const STEERED_NARRATIVE: &str = "Took the operator's steer on my second turn and reported.";
/// What a generation carrying a child's end reports.
const PUSHED_NARRATIVE: &str = "My grandchild ended; took its end as my next turn and reported.";
const PUSH_CALL_PREFIX: &str = "call_continuation_push";

const CHILD_TIMEOUT_SECS: u64 = 120;
/// Short enough to wait out in run 2, long enough that the child's first turn cannot eat it.
const SHORT_CHILD_TIMEOUT_SECS: u64 = 25;
const SHIM_TIMEOUT_SECS: u64 = 600;
const IDLE_GRACE: Duration = Duration::from_secs(600);
const BOUND: Duration = Duration::from_secs(180);

/// The child's first turn backgrounds a grandchild and stops without reporting; a turn that
/// carries the steer reports, and so does one that carries the grandchild's end. First match wins,
/// and a later generation's request carries every earlier marker (its history replays the earlier
/// turns), so the scripts are listed latest first. A script whose one call is already in the
/// history answers with its final text: a third generation after the steer's does not report.
fn script() -> Script {
    Script {
        nodes: vec![
            NodeScript {
                marker: STEER_MARKER.into(),
                call_prefix: STEER_CALL_PREFIX.into(),
                turns: vec![ScriptedCall::new(
                    "report",
                    json!({"narrative": STEERED_NARRATIVE}),
                )],
                final_text: "Reported after the steer.".into(),
            },
            NodeScript {
                marker: PUSH_MARKER.into(),
                call_prefix: PUSH_CALL_PREFIX.into(),
                turns: vec![ScriptedCall::new(
                    "report",
                    json!({"narrative": PUSHED_NARRATIVE}),
                )],
                final_text: "Reported after my grandchild's end.".into(),
            },
            NodeScript {
                marker: DELEGATOR_MARKER.into(),
                call_prefix: CHILD_CALL_PREFIX.into(),
                turns: vec![ScriptedCall::new(
                    "spawn",
                    json!({
                        "agent_type": "codex-impl",
                        "prompt": GRANDCHILD_PROMPT,
                        "acceptance_criteria": ["the grandchild was released"],
                        "writable_scope": ["src/**"],
                        "timeout_secs": SHIM_TIMEOUT_SECS,
                        "background": true,
                    }),
                )],
                final_text: "Backgrounded a grandchild; ending my turn.".into(),
            },
        ],
        ..Script::default()
    }
}

/// A `codex` that runs the real binary for the child under test — its first launch and every
/// continuation of it — logging each such argv (record-separated, since a rendered message spans
/// lines), sleeps on the wedge marker, and blocks every other
/// invocation on its gate file.
fn shim(dir: &Path, bed: &Gates, real: &Path) -> PathBuf {
    let bin = dir.join("codex");
    let script = format!(
        r#"#!/bin/sh
case "$1" in
  --version) echo "codex-cli 0.146.0-marion-continuation-shim"; exit 0 ;;
esac
case "$*" in
  *{wedge}*) printf '%s\036' "$*" >> {argv_log}; exec sleep {shim_timeout} ;;
  *{sessionless}*) printf '%s\036' "$*" >> {argv_log}
    while [ ! -e {sessionless_gate} ]; do sleep 0.05; done
    echo "no frame names a thread here"; exit 0 ;;
  *{delegator}*|*{steer}*|*{push}*) printf '%s\036' "$*" >> {argv_log}; exec {real} "$@" ;;
  *{root}*) gate={root_gate} ;;
  *) gate={grandchild_gate} ;;
esac
waited=0
while [ ! -e "$gate" ]; do
  sleep 0.05
  waited=$((waited + 1))
  if [ "$waited" -gt 4000 ]; then exit 0; fi
done
exit 0
"#,
        wedge = WEDGE_MARKER,
        delegator = DELEGATOR_MARKER,
        steer = STEER_MARKER,
        push = common::shell_quote(Path::new(PUSH_MARKER)),
        sessionless = SESSIONLESS_MARKER,
        sessionless_gate = common::shell_quote(&bed.sessionless),
        root = ROOT_MARKER,
        shim_timeout = SHIM_TIMEOUT_SECS,
        real = common::shell_quote(real),
        argv_log = common::shell_quote(&bed.argv_log),
        root_gate = common::shell_quote(&bed.root),
        grandchild_gate = common::shell_quote(&bed.grandchild),
    );
    std::fs::write(&bin, script).expect("the shim is written");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
        .expect("the shim is executable");
    bin
}

fn which(program: &str) -> PathBuf {
    let out = std::process::Command::new("which")
        .arg(program)
        .output()
        .expect("`which` runs");
    assert!(
        out.status.success(),
        "this test drives a REAL {program}; put it on PATH"
    );
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
}

struct Owned {
    agent_id: AgentId,
    token: String,
}

fn spawn_over_socket(
    sup: &common::Supervisor,
    state: &Path,
    caller: Option<&Owned>,
    p: AgentSpawnParams,
) -> Owned {
    let p = AgentSpawnParams {
        caller: caller.map(|c| SpawnCaller {
            agent_id: c.agent_id.clone(),
            node_token: c.token.clone(),
        }),
        ..p
    };
    let answered = sup
        .call(Call::AgentSpawn(p))
        .unwrap_or_else(|e| panic!("agent/spawn must be served: {e}"));
    let MethodResult::AgentSpawn(r) = Method::AgentSpawn
        .decode_result(&answered)
        .expect("a well-formed agent/spawn result")
    else {
        panic!("agent/spawn answers with an agent/spawn result");
    };
    let token = common::declaration_of(state, &r.agent_id)
        .remove("MARION_NODE_TOKEN")
        .expect("declaration_of asserts the token is there");
    Owned {
        agent_id: r.agent_id,
        token,
    }
}

fn params(prompt: String, repo: Option<&Path>, timeout_secs: u64) -> AgentSpawnParams {
    AgentSpawnParams {
        agent_type: "codex-impl".into(),
        prompt,
        native_launch: None,
        caller: None,
        repo: repo.map(Path::to_path_buf),
        acceptance_criteria: vec![],
        verification: vec![],
        writable_scope: vec!["src/**".into()],
        timeout_secs: Some(timeout_secs),
        model: None,
        no_change_record: repo.map(|_| true),
        pane: None,
        isolation: None,
        allow_concurrent_writes: None,
        notify_parent: false,
    }
}

/// An operator's steer, answered: the message id its journal records carry.
fn steer(sup: &common::Supervisor, agent_id: &AgentId, text: String) -> String {
    let answered = sup
        .call(Call::NodeSteer(NodeSteerParams {
            agent_id: agent_id.clone(),
            text,
            caller: None,
        }))
        .unwrap_or_else(|e| panic!("node/steer must accept a message for a held node: {e}"));
    let MethodResult::NodeSteer(r) = Method::NodeSteer
        .decode_result(&answered)
        .expect("a well-formed node/steer result")
    else {
        panic!("node/steer answers with a delivery result");
    };
    assert!(r.queued);
    r.message_id.expect("a queued message has an id")
}

// ---- reading the journal back -------------------------------------------------------------------

fn tree(journal: &Path) -> Replay {
    read_path(journal).expect("the journal reads back")
}

fn state_of(journal: &Path, id: &AgentId) -> Option<NodeState> {
    tree(journal).get(id).map(|n| n.state)
}

fn is_exited(journal: &Path, id: &AgentId) -> bool {
    state_of(journal, id).is_some_and(|s| s.is_exited())
}

fn wait_for(journal: &Path, what: &str, mut fact: impl FnMut(&Path) -> bool) {
    let deadline = Instant::now() + BOUND;
    while !fact(journal) {
        assert!(
            Instant::now() < deadline,
            "{what} never became true within {BOUND:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn journal_lines(journal: &Path) -> Vec<Value> {
    std::fs::read_to_string(journal)
        .expect("the journal reads back")
        .lines()
        .map(|l| serde_json::from_str(l).expect("a journal line is JSON"))
        .collect()
}

/// Every record of `kind` about `agent_id`, in journal order.
fn records_of(journal: &Path, kind: &str, agent_id: &AgentId) -> Vec<Value> {
    journal_lines(journal)
        .into_iter()
        .filter(|r| {
            r.pointer(&format!("/kind/{kind}/agent_id"))
                .and_then(Value::as_str)
                == Some(agent_id.0.as_str())
        })
        .collect()
}

fn contract_of(state: &Path, id: &AgentId) -> Value {
    let all = persisted_contracts(state).expect("the state tree walks");
    let mine: Vec<&marion_testsupport::PersistedContract> = all
        .iter()
        .filter(|c| c.path.to_string_lossy().contains(&id.0))
        .collect();
    assert_eq!(mine.len(), 1, "one contract for the child under test");
    mine[0]
        .parsed
        .clone()
        .unwrap_or_else(|e| panic!("the child's contract does not read back: {e}"))
}

// ---- the bed --------------------------------------------------------------------------------------

struct Gates {
    root: PathBuf,
    grandchild: PathBuf,
    sessionless: PathBuf,
    argv_log: PathBuf,
}

struct Bed {
    dir: marion_testsupport::Scratch,
    state: PathBuf,
    journal: PathBuf,
    gates: Gates,
    reqlog: PathBuf,
    server: Option<CannedServer>,
    sup: common::Supervisor,
    child: Owned,
}

impl Bed {
    fn start(tag: &str, child_timeout_secs: u64) -> Bed {
        Bed::start_with(
            tag,
            child_timeout_secs,
            format!("{DELEGATOR_MARKER}: background a grandchild, then stop."),
        )
    }

    fn start_with(tag: &str, child_timeout_secs: u64, child_prompt: String) -> Bed {
        assert!(
            on_path("codex"),
            "this test drives a REAL codex child; put `codex` ({}) on PATH",
            pinned_version("codex")
        );
        let dir = scratch(&format!("continuation-{tag}"));
        let repo = fixture_repo(&dir);
        let state = dir.join("state");
        let shim_dir = dir.join("bin");
        let gates = Gates {
            root: dir.join("root-gate"),
            grandchild: dir.join("grandchild-gate"),
            sessionless: dir.join("sessionless-gate"),
            argv_log: dir.join("child-argv.log"),
        };
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&shim_dir).unwrap();
        shim(&shim_dir, &gates, &which("codex"));

        let reqlog = dir.join("provider-requests.jsonl");
        let server = CannedServer::start(Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog: reqlog.clone(),
            script: script(),
        })
        .expect("the canned provider binds");

        let path_env = format!(
            "{}:{}",
            shim_dir.to_string_lossy(),
            std::env::var("PATH").unwrap_or_default()
        );
        let key = project_root(&repo);
        let project = ProjectDir::new(&state, &key);
        let sup =
            common::Supervisor::start(&state, &key, &path_env, &server.base_url(), IDLE_GRACE);
        let root = spawn_over_socket(
            &sup,
            &state,
            None,
            params(
                format!("{ROOT_MARKER}: hold the tree open"),
                Some(&repo),
                SHIM_TIMEOUT_SECS,
            ),
        );
        let child = spawn_over_socket(
            &sup,
            &state,
            Some(&root),
            params(child_prompt, None, child_timeout_secs),
        );
        Bed {
            journal: project.journal(),
            dir,
            state,
            gates,
            reqlog,
            server: Some(server),
            sup,
            child,
        }
    }

    /// The child held under §7.6, which is the turn boundary a message is taken at.
    fn wait_for_hold(&self) {
        let child = self.child.agent_id.clone();
        wait_for(&self.journal, "the child's Blocked(Descendants)", |j| {
            assert!(!is_exited(j, &child), "the child exited before it was held");
            state_of(j, &child) == Some(NodeState::Blocked(BlockReason::Descendants))
        });
    }

    fn child_argvs(&self) -> Vec<String> {
        std::fs::read_to_string(&self.gates.argv_log)
            .unwrap_or_default()
            .split('\u{1e}')
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .collect()
    }
}

impl Drop for Bed {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.gates.grandchild, b"go");
        let _ = std::fs::write(&self.gates.sessionless, b"go");
        let _ = std::fs::write(&self.gates.root, b"go");
        let needle = self.dir.to_string_lossy().into_owned();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !survivors(&needle).is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.sup.stop();
        drop(self.server.take());
        for (pid, _) in survivors(&needle) {
            kill_hard(pid);
        }
    }
}

// ---- the runs -------------------------------------------------------------------------------------

/// The payload of every `kind` record about `agent_id`, in journal order.
fn payloads(journal: &Path, kind: &str, agent_id: &AgentId) -> Vec<Value> {
    records_of(journal, kind, agent_id)
        .into_iter()
        .map(|r| r["kind"][kind].clone())
        .collect()
}

/// `(message_id, via)` of every `MessageDelivered` about `agent_id`.
fn deliveries(journal: &Path, agent_id: &AgentId) -> Vec<(String, String)> {
    payloads(journal, "MessageDelivered", agent_id)
        .iter()
        .map(|d| {
            (
                d["message_id"].as_str().unwrap_or_default().to_string(),
                d["via"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn thread_of(journal: &Path, id: &AgentId) -> String {
    tree(journal)
        .get(id)
        .and_then(|n| n.harness_session.clone())
        .expect("codex named its thread on the first generation")
}

/// Every provider request whose body carries `needle`.
fn requests_carrying(reqlog: &Path, needle: &str) -> Vec<Value> {
    RequestLog::read(reqlog)
        .expect("the request log reads")
        .into_iter()
        .filter(|r| r["body"].to_string().contains(needle))
        .collect()
}

/// **An operator's steer during a §7.6 hold is the held codex child's next turn**: a relaunch
/// under the thread codex named, resolving the message `continuation:gen2`. Generation two reports
/// with the grandchild still live, so §7.6 accepts it at once as reported early.
#[test]
fn a_steer_during_a_descendant_hold_resumes_the_codex_child_as_its_second_generation() {
    let bed = Bed::start("held-steer", CHILD_TIMEOUT_SECS);
    let child = bed.child.agent_id.clone();
    bed.wait_for_hold();

    let text = format!("{STEER_MARKER}: report what you have now");
    let message_id = steer(&bed.sup, &child, text.clone());
    wait_for(&bed.journal, "the child's Exited", |j| is_exited(j, &child));

    // One node, two process lifetimes: the relaunch is a generation of the same id.
    let node = tree(&bed.journal).get(&child).cloned().expect("the child");
    assert_eq!(
        node.spawn_generation, 2,
        "the continuation is the node's second generation"
    );
    assert_eq!(records_of(&bed.journal, "Spawned", &child).len(), 2);
    let thread = thread_of(&bed.journal, &child);

    // The relaunch is the row's resume spelling with the rendered message as the prompt. (The
    // canned run declares marion in the codex config document both generations read; a live
    // resume's `-c` redeclaration is pinned in `marion-harness`.)
    let argvs = bed.child_argvs();
    assert_eq!(
        argvs.len(),
        2,
        "one launch and one continuation: {argvs:#?}"
    );
    let rendered = format!("marion: message from the operator: {text}");
    assert!(
        argvs[1].contains(&format!("resume {thread}")) && argvs[1].contains(&rendered),
        "generation two resumes thread {thread} with the rendered steer: {}",
        argvs[1]
    );

    // A real resume: the provider saw the first turn's calls replayed beside the message.
    let steered = requests_carrying(&bed.reqlog, STEER_MARKER);
    assert!(!steered.is_empty(), "generation two reached the provider");
    assert!(
        steered.iter().all(|r| r["body"]
            .to_string()
            .contains(&format!("{CHILD_CALL_PREFIX}_00"))),
        "every generation-two request carries the first generation's turn"
    );

    // The message is resolved by the lane's verb, once.
    assert_eq!(
        deliveries(&bed.journal, &child),
        vec![(message_id, "continuation:gen2".to_string())]
    );
    assert!(records_of(&bed.journal, "MessageDropped", &child).is_empty());

    // The contract is the last reporting generation's.
    let completion = contract_of(&bed.state, &child)["completion"].clone();
    assert_eq!(completion["status"], json!("Ok"), "{completion}");
    assert!(
        completion["narrative"]
            .to_string()
            .contains(STEERED_NARRATIVE),
        "the report generation two made is the contract's: {completion}"
    );
    assert_eq!(
        completion["reported_early"],
        json!(true),
        "the grandchild is still live, and generation two reported: {completion}"
    );
    assert_eq!(completion["held_to_timeout"], json!(false), "{completion}");
}

/// **A child's end resumes its held codex parent**: the parent stopped unreported with a
/// background child live, so §7.6 holds it; the child's end, queued into the parent's inbox, ends
/// the hold as the parent's second generation, which reads it and reports.
#[test]
fn a_childs_end_during_a_descendant_hold_resumes_the_codex_parent() {
    let bed = Bed::start("held-push", CHILD_TIMEOUT_SECS);
    let parent = bed.child.agent_id.clone();
    bed.wait_for_hold();

    std::fs::write(&bed.gates.grandchild, b"go").unwrap();
    wait_for(&bed.journal, "the parent's Exited", |j| {
        is_exited(j, &parent)
    });

    let node = tree(&bed.journal)
        .get(&parent)
        .cloned()
        .expect("the parent");
    assert_eq!(
        node.spawn_generation, 2,
        "the child's end was a second turn"
    );
    let thread = thread_of(&bed.journal, &parent);
    let argvs = bed.child_argvs();
    assert_eq!(argvs.len(), 2, "{argvs:#?}");
    assert!(
        argvs[1].contains(&format!("resume {thread}")) && argvs[1].contains(PUSH_MARKER),
        "generation two resumes thread {thread} with the rendered end: {}",
        argvs[1]
    );
    assert!(
        !requests_carrying(&bed.reqlog, PUSH_MARKER).is_empty(),
        "the model read the child's end"
    );

    let queued = payloads(&bed.journal, "MessageQueued", &parent);
    assert_eq!(queued.len(), 1, "{queued:#?}");
    assert_eq!(
        deliveries(&bed.journal, &parent),
        vec![(
            queued[0]["message_id"].as_str().unwrap().to_string(),
            "continuation:gen2".to_string()
        )]
    );

    let completion = contract_of(&bed.state, &parent)["completion"].clone();
    assert_eq!(completion["status"], json!("Ok"), "{completion}");
    assert!(
        completion["narrative"]
            .to_string()
            .contains(PUSHED_NARRATIVE),
        "{completion}"
    );
    assert_eq!(completion["reported_early"], json!(false), "{completion}");
}

/// **No observed session, no continuation**: a steer queued while a codex child runs whose stream
/// never names its thread is dropped at the child's stop, with that reason, and the node ends on
/// its one generation.
#[test]
fn a_message_for_a_node_whose_stream_named_no_session_is_dropped_with_the_reason() {
    let bed = Bed::start_with(
        "sessionless",
        CHILD_TIMEOUT_SECS,
        format!("{SESSIONLESS_MARKER}: run without naming a thread"),
    );
    let child = bed.child.agent_id.clone();
    wait_for(&bed.journal, "the child's Running", |j| {
        state_of(j, &child) == Some(NodeState::Running)
    });
    let message_id = steer(&bed.sup, &child, "this has nowhere to go".into());
    std::fs::write(&bed.gates.sessionless, b"go").unwrap();
    wait_for(&bed.journal, "the child's Exited", |j| is_exited(j, &child));

    let dropped = payloads(&bed.journal, "MessageDropped", &child);
    assert_eq!(dropped.len(), 1, "{dropped:#?}");
    assert_eq!(dropped[0]["message_id"], json!(message_id));
    assert!(
        dropped[0]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("session")),
        "{dropped:#?}"
    );
    assert!(deliveries(&bed.journal, &child).is_empty());
    assert_eq!(records_of(&bed.journal, "Spawned", &child).len(), 1);
}

/// **The wall clock is the node's**: a continuation launched late in the bound is killed when the
/// first generation's bound runs out, not a fresh bound after its own launch.
#[test]
fn a_continuation_runs_on_what_is_left_of_the_nodes_own_wall_clock() {
    let bed = Bed::start("shared-clock", SHORT_CHILD_TIMEOUT_SECS);
    let child = bed.child.agent_id.clone();
    bed.wait_for_hold();

    steer(&bed.sup, &child, format!("{WEDGE_MARKER}: take forever"));
    wait_for(&bed.journal, "the child's Exited", |j| is_exited(j, &child));

    let spawns = records_of(&bed.journal, "Spawned", &child);
    assert_eq!(spawns.len(), 2, "the wedged continuation was launched");
    // One supervisor wrote both, so its monotonic clock orders and measures them.
    let mono = |r: &Value| -> u64 {
        r["mono_ns"]
            .as_u64()
            .unwrap_or_else(|| panic!("a record carries its monotonic time: {r}"))
    };
    let exited = records_of(&bed.journal, "Exited", &child);
    assert_eq!(exited.len(), 1);
    let lived = Duration::from_nanos(mono(&exited[0]).saturating_sub(mono(&spawns[0])));
    assert!(
        lived < Duration::from_secs(SHORT_CHILD_TIMEOUT_SECS + 10),
        "the node ended {lived:?} after its first launch; its one bound is \
         {SHORT_CHILD_TIMEOUT_SECS}s, so generation two had only what was left of it"
    );
    let completion = contract_of(&bed.state, &child)["completion"].clone();
    assert_eq!(
        completion["status"],
        json!("TimedOut"),
        "generation two was killed on the node's bound: {completion}"
    );
}

// ---- the root site --------------------------------------------------------------------------------

const ROOT_PUSH_MARKER: &str = "MARION-CONTINUATION-ROOT-PUSH-81c3";
const ROOT_CHILD_MARKER: &str = "MARION-CONTINUATION-ROOT-CHILD-81c3";
const ROOT_TOOK_THE_END: &str = "Took my child's end as my second turn.";

/// **A `LaunchOnly` root is continued the same way**: a codex root under `marion run` backgrounds
/// a codex child and ends its turn; marion holds the root while the child's end is owed, relaunches
/// it under its thread with the rendered end as the prompt, journals the message delivered by that
/// second generation, and only then ends the run.
#[test]
fn a_codex_roots_background_childs_end_is_its_second_generation() {
    assert!(
        on_path("codex"),
        "this test drives a REAL codex root; put `codex` ({}) on PATH",
        pinned_version("codex")
    );
    let dir = scratch("continuation-root");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let reqlog = dir.join("provider-requests.jsonl");
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: reqlog.clone(),
        script: Script {
            // Latest turn first: the root's second request replays its first.
            nodes: vec![
                NodeScript {
                    marker: PUSH_MARKER.into(),
                    call_prefix: "call_continuation_root_push".into(),
                    turns: vec![],
                    final_text: ROOT_TOOK_THE_END.into(),
                },
                NodeScript {
                    marker: ROOT_CHILD_MARKER.into(),
                    call_prefix: "call_continuation_root_child".into(),
                    turns: vec![ScriptedCall::new(
                        "report",
                        json!({"narrative": "the root's child reported"}),
                    )],
                    final_text: "Reported.".into(),
                },
                NodeScript {
                    marker: ROOT_PUSH_MARKER.into(),
                    call_prefix: "call_continuation_root".into(),
                    turns: vec![ScriptedCall::new(
                        "spawn",
                        json!({
                            "agent_type": "codex-impl",
                            "prompt": format!("{ROOT_CHILD_MARKER}: report at once"),
                            "acceptance_criteria": ["it reported"],
                            "writable_scope": ["src/**"],
                            "timeout_secs": CHILD_TIMEOUT_SECS,
                            "background": true,
                        }),
                    )],
                    final_text: "Backgrounded a child; ending my turn.".into(),
                },
            ],
            ..Script::default()
        },
    })
    .expect("the canned provider binds");

    let out = marion_supervisor::run::run_bounded(
        std::process::Command::new(env!("CARGO_BIN_EXE_marion"))
            .args([
                "run",
                "codex",
                "--prompt",
                &format!("{ROOT_PUSH_MARKER}: background a child, then stop."),
                "--repo",
                &repo.to_string_lossy(),
                "--state-dir",
                &state.to_string_lossy(),
                "--canned",
                "--base-url",
                &server.base_url(),
                "--timeout",
                "120",
            ])
            .current_dir(&dir),
        BOUND,
    )
    .expect("marion run starts");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.timed_out, "marion run hung\nstderr:\n{stderr}");
    assert_eq!(out.code, Some(0), "stderr:\n{stderr}");

    let journal = ProjectDir::new(&state, &project_root(&repo)).journal();
    let replay = tree(&journal);
    let root = replay
        .nodes()
        .iter()
        .find(|n| n.depth() == Some(0))
        .cloned()
        .expect("a root");
    assert_eq!(root.spawn_generation, 2, "{root:#?}");
    let thread = root
        .harness_session
        .clone()
        .expect("codex named its thread");
    let pushed = requests_carrying(&reqlog, PUSH_MARKER);
    assert!(
        !pushed.is_empty(),
        "the root's second generation read the end"
    );
    assert!(
        pushed
            .iter()
            .all(|r| r["body"].to_string().contains("call_continuation_root_00")),
        "on thread {thread}, replaying the first generation's turn"
    );
    let delivered = deliveries(&journal, &root.agent_id);
    assert_eq!(delivered.len(), 1, "{delivered:#?}");
    assert_eq!(delivered[0].1, "continuation:gen2");
}
