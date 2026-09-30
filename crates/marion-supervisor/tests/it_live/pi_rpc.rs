//! **pi over its JSONL command channel (`--mode rpc`), end to end** through `marion run` and its
//! supervisor, every model call served by the canned provider (no paid tokens).
//!
//! pi's row selects `Surfaces::JsonlRpc` with its own vocabulary (`marion_harness::pi::RPC`), so a
//! pi node is driven by the same typed-turn loop as a headless claude node, in pi's words. Four
//! properties, each observed on the provider's own request log or on the node's own stream:
//!
//! 1. an operator's `node/steer` into a running pi **child**, written while one of its requests is
//!    held at the provider, is read in the **next request of the same turn** (S34
//!    `pi-rpc-steer-mid-tool`'s fold), journaled once, `jsonl-rpc:mid-turn`;
//! 2. a pi child whose wall clock expires mid-request is **aborted** — pi ends the turn
//!    `stopReason: "aborted"` and exits on its own — rather than killed, and its contract is
//!    `TimedOut`;
//! 3. a background child that ends while its pi parent is **held** reaches the parent in the same
//!    process as its next turn, once, `jsonl-rpc:next-turn`.
//! 4. a pi parent peeks at a background child with `status` and collects it with `wait`.
//!
//! ```sh
//! cargo test -p marion-supervisor --test it_live pi_rpc::
//! ```

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_core::journal::RecordKind;
use marion_core::paths::ProjectDir;
use marion_core::proto::params::NodeSteerParams;
use marion_core::proto::{Call, Outcome};
use marion_provider::{CannedServer, Config, Hold, NodeScript, Script, ScriptedCall};
use marion_testsupport::{fixture_repo, scratch};
use serde_json::{Value, json};

use crate::common;

use common::run::finish;

/// How long any one wait here may take — two pi nodes' whole runs, root and child, from their
/// rows ([`common::boot::run`]). It exists so a hang fails instead of wedging the suite.
fn run_bound() -> Duration {
    2 * common::boot::run(PI)
}

/// The one agent type every node here is.
const PI: &str = "pi-orchestrator";

/// A hold on the OpenAI-wire requests `pred` picks, released by the test. The request is logged
/// before it is held, so "held" is a fact in the provider's log.
struct Held {
    pred: Box<dyn Fn(&Value) -> bool + Send + Sync>,
    parked: AtomicU64,
    released: Mutex<bool>,
    wake: Condvar,
}

impl std::fmt::Debug for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Held")
            .field("parked", &self.parked)
            .finish()
    }
}

impl Held {
    fn on(pred: impl Fn(&Value) -> bool + Send + Sync + 'static) -> Arc<Held> {
        Arc::new(Held {
            pred: Box::new(pred),
            parked: AtomicU64::new(0),
            released: Mutex::new(false),
            wake: Condvar::new(),
        })
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

impl Hold for Held {
    fn wait_for(&self, wire: Option<&str>, body: &Value) {
        if wire != Some("openai") || !(self.pred)(body) {
            return;
        }
        let mut released = self.released.lock().unwrap();
        self.parked.fetch_add(1, Ordering::SeqCst);
        while !*released {
            released = self.wake.wait(released).unwrap();
        }
    }
}

fn carries(v: &Value, needle: &str) -> bool {
    v.to_string().contains(needle)
}

/// A repo, a state dir, and a canned provider holding what `hold` picks.
struct Bed {
    dir: marion_testsupport::Scratch,
    repo: PathBuf,
    state: PathBuf,
    server: CannedServer,
    path: String,
}

impl Bed {
    /// `None` (announced) on a runner that declared it has no harnesses.
    fn new(tag: &str, nodes: Vec<NodeScript>, hold: Arc<Held>) -> Option<Bed> {
        if !marion_testsupport::harness_available("pi") {
            return None;
        }
        let dir = scratch(tag);
        let repo = fixture_repo(&dir);
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let server = CannedServer::start_held(
            Config {
                addr: ([127, 0, 0, 1], 0).into(),
                reqlog: dir.join("provider-requests.jsonl"),
                script: Script {
                    nodes,
                    ..Script::default()
                },
            },
            Some(hold as Arc<dyn Hold>),
        )
        .expect("the canned provider binds");
        Some(Bed {
            dir,
            repo,
            state,
            server,
            path: std::env::var("PATH").unwrap_or_default(),
        })
    }

    fn project(&self) -> ProjectDir {
        ProjectDir::new(
            &self.state,
            &marion_supervisor::socket::project_root(&self.repo),
        )
    }

    fn records(&self) -> Vec<RecordKind> {
        common::journal::records(&self.project().journal())
    }

    /// The parentless node `marion run` launched, once its intent is journaled.
    fn root(&self) -> Option<AgentId> {
        self.records().into_iter().find_map(|k| match k {
            RecordKind::SpawnIntent(i) if i.parent_id.is_none() => Some(i.agent_id),
            _ => None,
        })
    }

    /// The root's first child, once its intent is journaled.
    fn child(&self) -> Option<AgentId> {
        self.records().into_iter().find_map(|k| match k {
            RecordKind::SpawnIntent(i) if i.parent_id.is_some() => Some(i.agent_id),
            _ => None,
        })
    }

    fn events_of(&self, agent: &AgentId) -> String {
        std::fs::read_to_string(self.project().agent(agent).events()).unwrap_or_default()
    }

    fn run(&self, agent_type: &str, prompt: &str) -> Child {
        Command::new(env!("CARGO_BIN_EXE_marion"))
            .args([
                "run",
                agent_type,
                "--prompt",
                prompt,
                "--repo",
                &self.repo.to_string_lossy(),
                "--state-dir",
                &self.state.to_string_lossy(),
                "--base-url",
                &self.server.base_url(),
                "--canned",
                // A wall clock over the whole run (§9), on this duplex root as on every node.
                "--timeout",
                "150",
            ])
            .current_dir(&self.dir)
            .env("PATH", &self.path)
            .env("COPILOT_AUTO_UPDATE", "false")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(self.dir.join("run.stderr")).unwrap())
            .spawn()
            .expect("marion run starts")
    }

    /// What the run left so far, for a failure message: `marion run`'s stderr and the provider log.
    fn evidence(&self) -> String {
        let stderr = std::fs::read_to_string(self.dir.join("run.stderr")).unwrap_or_default();
        let log = std::fs::read_to_string(self.server.reqlog_path()).unwrap_or_default();
        format!("marion run stderr:\n{stderr}\nprovider log:\n{log}")
    }

    /// [`wait_until`], with the run's evidence on failure.
    fn wait(&self, what: &str, mut cond: impl FnMut() -> bool) {
        let until = Instant::now() + run_bound();
        while !cond() {
            assert!(
                Instant::now() < until,
                "timed out waiting for {what}\n{}",
                self.evidence()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.server.requests().expect("the request log reads")
    }

    fn delivered(&self, agent: &AgentId) -> Vec<String> {
        common::journal::delivered_to(&self.project().journal(), agent)
            .into_iter()
            .map(|(_, via)| via)
            .collect()
    }
}

/// A pi root whose one call is a blocking `spawn` of a pi child with `child_prompt`.
fn delegating_root(marker: &str, child_prompt: String, timeout_secs: u64) -> NodeScript {
    NodeScript {
        marker: marker.into(),
        call_prefix: format!("{}root", marker.to_lowercase().replace('-', "")),
        turns: vec![ScriptedCall::new(
            "mcp__marion__spawn",
            json!({
                "agent_type": "pi-orchestrator",
                "prompt": child_prompt,
                // pi validates a call against the declared schema, which requires it.
                "acceptance_criteria": [],
                "timeout_secs": timeout_secs,
            }),
        )],
        final_text: "The child is done.".into(),
    }
}

const STEER_ROOT: &str = "MARION-PI-STEER-ROOT-4b17";
const STEER_CHILD: &str = "MARION-PI-STEER-CHILD-90ce";
const STEER_PREFIX: &str = "pisteer";
const STEER_TEXT: &str = "MARION-PI-STEER-TEXT-2d5a: also mention the docs";

/// **An operator's steer into a running pi child is folded into its running turn**: written while
/// the child's second request is held, it is in the third request — after the second call's
/// result, in the same turn — and not in the held one.
#[test]
fn a_steer_into_a_running_pi_child_is_read_in_the_next_request_of_the_same_turn() {
    let first = format!("{STEER_PREFIX}_00");
    let second = format!("{STEER_PREFIX}_01");
    let (f, s) = (first.clone(), second.clone());
    let hold = Held::on(move |b| {
        carries(b, STEER_CHILD) && !carries(b, STEER_ROOT) && carries(b, &f) && !carries(b, &s)
    });
    let Some(bed) = Bed::new(
        "pi-steer",
        vec![
            delegating_root(
                STEER_ROOT,
                format!("{STEER_CHILD}: list the tree twice."),
                120,
            ),
            NodeScript {
                marker: STEER_CHILD.into(),
                call_prefix: STEER_PREFIX.into(),
                turns: vec![
                    ScriptedCall::new("mcp__marion__list", json!({})),
                    ScriptedCall::new("mcp__marion__list", json!({})),
                ],
                final_text: "Listed twice. Done.".into(),
            },
        ],
        Arc::clone(&hold),
    ) else {
        return;
    };
    let run = bed.run(
        "pi-orchestrator",
        &format!("{STEER_ROOT}: delegate one child."),
    );
    bed.wait("the child's second request to be held", || {
        hold.parked.load(Ordering::SeqCst) == 1
    });
    let child = bed.child().expect("the child's intent is journaled");

    let mut client =
        common::client::Client::dial(&common::client::paths_for(&bed.state, &bed.repo));
    let id = client.send(Call::NodeSteer(NodeSteerParams {
        agent_id: child.clone(),
        text: STEER_TEXT.into(),
        caller: None,
    }));
    let (_, outcome) = client.read_to_response(id);
    let Outcome::Result(body) = outcome else {
        panic!("node/steer was refused: {outcome:?}")
    };
    assert_eq!(body["queued"], json!(true), "{body}");
    bed.wait("the steer to be delivered mid-turn", || {
        !bed.delivered(&child).is_empty()
    });
    hold.release();
    finish(run, run_bound(), &bed.dir.join("run.stderr"));

    let child_requests: Vec<Value> = bed
        .requests()
        .into_iter()
        .filter(|r| {
            carries(&r["body"], STEER_CHILD)
                && !carries(&r["body"], STEER_ROOT)
                && carries(&r["body"], &first)
        })
        .collect();
    let held = child_requests
        .iter()
        .find(|r| !carries(&r["body"], &second))
        .expect("the held second request is logged");
    assert!(
        !carries(&held["body"], STEER_TEXT),
        "the steer cannot be in the request that was already in flight"
    );
    let next = child_requests
        .iter()
        .find(|r| carries(&r["body"], &second))
        .expect("the turn went on to a third request");
    assert!(
        carries(&next["body"], STEER_TEXT),
        "the model reads the steer in the request that carries the second call's result — the \
         same turn (request log: {})",
        bed.server.reqlog_path().display()
    );
    // The steer, once, into the running turn; then, because this child never calls `report`,
    // marion's one request for it as the next turn (§7.6's grace turn).
    assert_eq!(
        bed.delivered(&child),
        ["jsonl-rpc:mid-turn", "jsonl-rpc:next-turn"],
        "the steer delivered once, into the running turn, then the one report request"
    );
}

const ABORT_ROOT: &str = "MARION-PI-ABORT-ROOT-6f02";
const ABORT_CHILD: &str = "MARION-PI-ABORT-CHILD-b3e9";

/// **A pi child past its wall clock is aborted, not killed.** Its first request is held beyond its
/// bound; marion writes pi's `abort`, pi ends the turn `aborted` and exits when stdin closes, and
/// the contract is `TimedOut`.
///
/// The bound is pi's boot budget and not a short constant: the wall clock runs from the spawn, so
/// the request is held only once pi has booted — and a 5 s bound expired mid-boot under load, which
/// is marion refusing a child that never connected, not aborting one mid-request.
#[test]
fn a_pi_child_past_its_wall_clock_is_aborted_and_times_out_without_a_kill() {
    let hold = Held::on(|b| carries(b, ABORT_CHILD) && !carries(b, ABORT_ROOT));
    let Some(bed) = Bed::new(
        "pi-abort",
        vec![
            delegating_root(
                ABORT_ROOT,
                format!("{ABORT_CHILD}: think for a long time."),
                common::boot::budget(PI).as_secs(),
            ),
            NodeScript {
                marker: ABORT_CHILD.into(),
                call_prefix: "piabortchild".into(),
                turns: vec![],
                final_text: "Too late.".into(),
            },
        ],
        Arc::clone(&hold),
    ) else {
        return;
    };
    let run = bed.run(
        "pi-orchestrator",
        &format!("{ABORT_ROOT}: delegate one child."),
    );
    bed.wait("the child's request to be held", || {
        hold.parked.load(Ordering::SeqCst) == 1
    });
    let child = bed.child().expect("the child's intent is journaled");
    bed.wait("the child's turn to end", || {
        bed.events_of(&child).contains(r#""type":"agent_end""#)
    });
    let events = bed.events_of(&child);
    hold.release();
    finish(run, run_bound(), &bed.dir.join("run.stderr"));

    assert!(
        events.contains(r#""stopReason":"aborted""#),
        "pi ended the turn because marion aborted it, not because it was killed:\n{events}"
    );
    let exited = bed
        .records()
        .into_iter()
        .find_map(|k| match k {
            RecordKind::Exited(e) if e.agent_id == child => Some(e),
            _ => None,
        })
        .expect("the child's exit is journaled");
    assert_eq!(
        serde_json::to_value(exited.status).unwrap(),
        json!("TimedOut"),
        "{exited:?}"
    );
    assert_eq!(
        exited.exit.signal, None,
        "the child exited on its own: {exited:?}"
    );
}

const PUSH_ROOT: &str = "MARION-PI-PUSH-ROOT-1c84";
const PUSH_CHILD: &str = "MARION-PI-PUSH-CHILD-7a3f";
const ENDED: &str = "you backgrounded as task_id";

/// **A background child's end reaches its held pi parent in the same process as its next turn,
/// once.** The child is held at the provider until the parent's first turn is over, so the end
/// lands while the parent is held rather than mid-turn.
#[test]
fn a_background_childs_end_reaches_its_held_pi_parent_once_as_its_next_turn() {
    let hold = Held::on(|b| carries(b, PUSH_CHILD) && !carries(b, PUSH_ROOT));
    let Some(bed) = Bed::new(
        "pi-push",
        vec![
            NodeScript {
                marker: PUSH_ROOT.into(),
                call_prefix: "pipushroot".into(),
                turns: vec![ScriptedCall::new(
                    "mcp__marion__spawn",
                    json!({
                        "agent_type": "pi-orchestrator",
                        "prompt": format!("{PUSH_CHILD}: report back through marion."),
                        "acceptance_criteria": [],
                        "timeout_secs": 120,
                        "background": true,
                    }),
                )],
                final_text: "The child is running in the background.".into(),
            },
            NodeScript {
                marker: PUSH_CHILD.into(),
                call_prefix: "pipushchild".into(),
                turns: vec![ScriptedCall::new(
                    "mcp__marion__report",
                    json!({"narrative": "Reported from the background."}),
                )],
                final_text: "Reported.".into(),
            },
        ],
        Arc::clone(&hold),
    ) else {
        return;
    };
    let run = bed.run(
        "pi-orchestrator",
        &format!("{PUSH_ROOT}: start one background child."),
    );
    bed.wait("the child's first request to be held", || {
        hold.parked.load(Ordering::SeqCst) >= 1
    });
    let root = bed.root().expect("the root's intent is journaled");
    bed.wait("the root's first turn to end", || {
        bed.events_of(&root).contains(r#""type":"agent_end""#)
    });
    hold.release();
    finish(run, run_bound(), &bed.dir.join("run.stderr"));

    let last_root = bed
        .requests()
        .into_iter()
        .rfind(|r| carries(&r["body"], PUSH_ROOT))
        .expect("the root made requests");
    assert_eq!(
        last_root["body"].to_string().matches(ENDED).count(),
        1,
        "the model reads the child's end exactly once (request log: {})",
        bed.server.reqlog_path().display()
    );
    assert_eq!(
        bed.delivered(&root),
        ["jsonl-rpc:next-turn"],
        "delivered once, as the held parent's next turn in the same process"
    );
    let starts = bed
        .events_of(&root)
        .matches(r#""type":"agent_start""#)
        .count();
    assert_eq!(starts, 2, "two turns of one pi process");
}

const WAIT_ROOT: &str = "MARION-PI-WAIT-ROOT-5e02";
const WAIT_CHILD: &str = "MARION-PI-WAIT-CHILD-b8d1";
const WAIT_NARRATIVE: &str = "MARION-PI-WAIT-NARRATIVE-0f4c";

/// The `tool_execution_end` frames of `events`, by the tool they end: `(toolName, isError, text)`.
fn tool_ends(events: &str) -> Vec<(String, bool, String)> {
    events
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|e| e.pointer("/payload/Vendor/json").cloned())
        .filter(|f| f["type"] == "tool_execution_end")
        .map(|f| {
            (
                f["toolName"].as_str().unwrap_or_default().to_string(),
                f["isError"].as_bool().unwrap_or(true),
                f["result"]["content"].to_string(),
            )
        })
        .collect()
}

/// **A pi parent backgrounds a child, peeks at it with `status`, and collects it with `wait`**:
/// the three verbs a parent has, each through marion's extension. `status` and `wait` both answer
/// without error, `wait` hands back the child's own report, and the end is journaled delivered
/// once, by that `wait` or by the fold into the running turn that beat it, never as a turn of its
/// own.
#[test]
fn a_pi_parent_collects_its_background_child_with_status_and_wait() {
    let Some(bed) = Bed::new(
        "pi-wait",
        vec![
            NodeScript {
                marker: WAIT_ROOT.into(),
                call_prefix: "piwaitroot".into(),
                turns: vec![
                    ScriptedCall::new(
                        "mcp__marion__spawn",
                        json!({
                            "agent_type": "pi-orchestrator",
                            "prompt": format!("{WAIT_CHILD}: report back through marion."),
                            "acceptance_criteria": [],
                            "timeout_secs": 120,
                            "background": true,
                        }),
                    ),
                    // pi validates each call against its schema, which requires the id: the
                    // handle's, read out of the spawn's answer as a model would.
                    ScriptedCall::new(
                        "mcp__marion__status",
                        json!({"id": {marion_provider::HANDLE_KEY: 0}}),
                    ),
                    ScriptedCall::new(
                        "mcp__marion__wait",
                        json!({"id": {marion_provider::HANDLE_KEY: 0}}),
                    ),
                ],
                final_text: "The child is done.".into(),
            },
            NodeScript {
                marker: WAIT_CHILD.into(),
                call_prefix: "piwaitchild".into(),
                turns: vec![ScriptedCall::new(
                    "mcp__marion__report",
                    json!({"narrative": WAIT_NARRATIVE}),
                )],
                final_text: "Reported.".into(),
            },
        ],
        Held::on(|_| false),
    ) else {
        return;
    };
    let run = bed.run(
        "pi-orchestrator",
        &format!("{WAIT_ROOT}: start one background child and wait for it."),
    );
    finish(run, run_bound(), &bed.dir.join("run.stderr"));

    let root = bed.root().expect("the root's intent is journaled");
    let ends = tool_ends(&bed.events_of(&root));
    let names: Vec<&str> = ends.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "mcp__marion__spawn",
            "mcp__marion__status",
            "mcp__marion__wait"
        ],
        "{}",
        bed.evidence()
    );
    for (name, is_error, text) in &ends {
        assert!(!is_error, "{name} answered with an error: {text}");
    }
    assert!(
        ends[2].2.contains(WAIT_NARRATIVE),
        "wait hands back the child's own report: {}",
        ends[2].2
    );
    // Once: by the `wait` that collected it, or, where the child ended before that `wait` was in
    // flight, by the fold into the root's running turn (`jsonl-rpc:mid-turn`, the push a parent
    // gets when it is not waiting; both measured under load). An end is journaled delivered by
    // whichever came first.
    let delivered = bed.delivered(&root);
    assert!(
        delivered == ["wait"] || delivered == ["jsonl-rpc:mid-turn"],
        "the end is delivered once, by the wait or by the fold before it: {delivered:?}"
    );
    let child = bed.child().expect("the child's intent is journaled");
    assert!(
        bed.records().iter().any(|k| matches!(k,
            RecordKind::Exited(e) if e.agent_id == child
                && e.status == marion_core::contract::ExitStatus::Ok)),
        "the child ended Ok"
    );
}
