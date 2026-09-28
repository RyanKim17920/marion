//! **Turn delivery into a real headless `claude`**, end to end through `marion run` and its
//! supervisor, every model call served by the canned provider (no paid tokens).
//!
//! Two properties, each observed on the provider's own request log — what the harness sent the
//! model — rather than on anything marion says about itself:
//!
//! 1. an operator's `node/steer` into a running headless claude node, written while one of its
//!    requests is held at the provider, is read by the model in the **next request of the same
//!    turn** (S31 `p0a/b3`'s fold), and journaled delivered once, `stream-json:mid-turn`;
//! 2. a background child that ends while its headless claude parent is **held** (its turn over,
//!    the child's end owed) reaches the parent as its next turn, journaled delivered once,
//!    `stream-json:next-turn`, and reaches the model once — the bridge's channel frame is not a
//!    second copy.
//!
//! Both again on a real **`opencode acp`** root, the ACP row's typed turn: the steer is written as
//! a second `session/prompt` mid-turn and opencode folds it (`acp:mid-turn`), and the ended child —
//! a real codex, so an opencode parent with a codex child — is its next prompt (`acp:next-turn`).
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test turn_delivery
//! ```
//!
//! It drives the installed `claude`, `opencode` and `codex` through the suite's version gate, so
//! it runs only on releases `PINNED_HARNESSES` admits. S31 measured the fold and the queue on
//! claude 2.1.280 and opencode 1.18.32 (`tests/fixtures/s31-turn-delivery/`); each later admission
//! re-runs this file.

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

mod common;

use common::run::finish;

/// Generous; it exists so a hang fails instead of wedging the suite.
const RUN_BOUND: Duration = Duration::from_secs(240);

/// A hold on the requests `pred` picks, released by the test. The request is logged before it is
/// held (`CannedServer`'s order), so "held" is a fact in the provider's log.
struct Held {
    /// The wire whose requests `pred` is asked about: `anthropic` for claude, `openai` for
    /// opencode.
    wire: &'static str,
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
        Held::on_wire("anthropic", pred)
    }
    fn on_wire(
        wire: &'static str,
        pred: impl Fn(&Value) -> bool + Send + Sync + 'static,
    ) -> Arc<Held> {
        Arc::new(Held {
            wire,
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
        if wire != Some(self.wire) || !(self.pred)(body) {
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

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let until = Instant::now() + RUN_BOUND;
    while !cond() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The run's `PATH`, or `None` on a runner that declared it has no harnesses. The gate panics on a
/// `program` outside `PINNED_HARNESSES`' admitted list, so this runs on a release the admission
/// ritual re-checked — and `scripts/admit-harness.sh` re-runs this file when it admits one.
fn admitted_path(program: &str) -> Option<String> {
    if !marion_testsupport::harness_available(program) {
        return None;
    }
    Some(std::env::var("PATH").unwrap_or_default())
}

/// A test bed: a repo, a state dir, and a canned provider holding what `hold` picks.
struct Bed {
    dir: marion_testsupport::Scratch,
    repo: PathBuf,
    state: PathBuf,
    server: CannedServer,
    path: String,
    /// The agent type `marion run` launches as the root, and the `--timeout` it runs under.
    root_type: &'static str,
    root_timeout: &'static str,
}

impl Bed {
    /// A claude root. `None` (announced) on a runner with no harnesses.
    fn new(tag: &str, nodes: Vec<NodeScript>, hold: Arc<Held>) -> Option<Bed> {
        Bed::rooted(("claude", "5"), "claude", tag, nodes, hold)
    }

    /// A root of `root_type`, which runs `program`. `None` (announced) on a runner with no
    /// harnesses.
    fn rooted(
        (root_type, root_timeout): (&'static str, &'static str),
        program: &str,
        tag: &str,
        nodes: Vec<NodeScript>,
        hold: Arc<Held>,
    ) -> Option<Bed> {
        let dir = scratch(tag);
        let path = admitted_path(program)?;
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
            path,
            root_type,
            root_timeout,
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

    fn run(&self, prompt: &str) -> Child {
        Command::new(env!("CARGO_BIN_EXE_marion"))
            .args([
                "run",
                self.root_type,
                "--prompt",
                prompt,
                "--repo",
                &self.repo.to_string_lossy(),
                "--state-dir",
                &self.state.to_string_lossy(),
                "--base-url",
                &self.server.base_url(),
                "--canned",
                "--timeout",
                self.root_timeout,
            ])
            .current_dir(&self.dir)
            .env("PATH", &self.path)
            .stdout(Stdio::null())
            // A file, not a pipe: nobody reads a pipe until the run ends, and a full one would
            // block the renderer it belongs to.
            .stderr(std::fs::File::create(self.dir.join("run.stderr")).unwrap())
            .spawn()
            .expect("marion run starts")
    }

    fn requests(&self) -> Vec<Value> {
        self.server.requests().expect("the request log reads")
    }
}

fn delivered(bed: &Bed, agent: &AgentId) -> Vec<(String, String)> {
    common::journal::delivered_to(&bed.project().journal(), agent)
}

const STEER_ROOT: &str = "MARION-TD-STEER-ROOT-7c41";
const STEER_PREFIX: &str = "tdsteer";
const STEER_TEXT: &str = "MARION-TD-STEER-TEXT-e902: also mention the docs";

/// **An operator's steer into a running headless claude node is folded into its running turn**:
/// written while the turn's second request is held, it is in the third request — the same turn,
/// after the second call's result — and not in the held one.
#[test]
fn a_steer_into_a_running_headless_claude_node_is_read_in_the_next_request_of_the_same_turn() {
    let first = format!("{STEER_PREFIX}_00");
    let second = format!("{STEER_PREFIX}_01");
    let (f, s) = (first.clone(), second.clone());
    let hold = Held::on(move |b| carries(b, STEER_ROOT) && carries(b, &f) && !carries(b, &s));
    let Some(bed) = Bed::new(
        "td-steer",
        vec![NodeScript {
            marker: STEER_ROOT.into(),
            call_prefix: STEER_PREFIX.into(),
            turns: vec![
                ScriptedCall::new("mcp__marion__list", json!({})),
                ScriptedCall::new("mcp__marion__list", json!({})),
            ],
            final_text: "Listed twice. Done.".into(),
        }],
        Arc::clone(&hold),
    ) else {
        return;
    };
    let run = bed.run(&format!("{STEER_ROOT}: list the tree twice."));
    wait_until("the root's second request to be held", || {
        hold.parked.load(Ordering::SeqCst) == 1
    });
    let root = bed.root().expect("the root's intent is journaled");

    let mut client =
        common::client::Client::dial(&common::client::paths_for(&bed.state, &bed.repo));
    let id = client.send(Call::NodeSteer(NodeSteerParams {
        agent_id: root.clone(),
        text: STEER_TEXT.into(),
        caller: None,
    }));
    let (_, outcome) = client.read_to_response(id);
    let Outcome::Result(body) = outcome else {
        panic!("node/steer was refused: {outcome:?}")
    };
    assert_eq!(body["queued"], json!(true), "{body}");
    // Written into the running turn while its request is still held.
    wait_until("the steer to be delivered mid-turn", || {
        !delivered(&bed, &root).is_empty()
    });
    hold.release();
    finish(run, RUN_BOUND, &bed.dir.join("run.stderr"));

    let root_requests: Vec<Value> = bed
        .requests()
        .into_iter()
        .filter(|r| carries(&r["body"], STEER_ROOT) && carries(&r["body"], &first))
        .collect();
    let held = root_requests
        .iter()
        .find(|r| !carries(&r["body"], &second))
        .expect("the held second request is logged");
    assert!(
        !carries(&held["body"], STEER_TEXT),
        "the steer cannot be in the request that was already in flight"
    );
    let next = root_requests
        .iter()
        .find(|r| carries(&r["body"], &second))
        .expect("the turn went on to a third request");
    assert!(
        carries(&next["body"], STEER_TEXT),
        "the model reads the steer in the request that carries the second call's result — the \
         same turn, not a turn of its own (request log: {})",
        bed.server.reqlog_path().display()
    );
    assert_eq!(
        delivered(&bed, &root)
            .into_iter()
            .map(|(_, via)| via)
            .collect::<Vec<_>>(),
        ["stream-json:mid-turn"],
        "delivered once, into the running turn"
    );
}

const PUSH_ROOT: &str = "MARION-TD-PUSH-ROOT-31d8";
const PUSH_CHILD: &str = "MARION-TD-PUSH-CHILD-5a60";
const ENDED: &str = "you backgrounded as task_id";

/// **A background child's end reaches its held headless claude parent as the parent's next turn,
/// once.** The child is held at the provider until the parent's first turn is over, so the end
/// lands while the parent is held rather than mid-turn.
#[test]
fn a_background_childs_end_reaches_its_held_headless_claude_parent_once_as_its_next_turn() {
    let hold = Held::on(|b| carries(b, PUSH_CHILD) && !carries(b, PUSH_ROOT));
    let Some(bed) = Bed::new(
        "td-push",
        vec![
            NodeScript {
                marker: PUSH_ROOT.into(),
                call_prefix: "tdpushroot".into(),
                turns: vec![ScriptedCall::new(
                    "mcp__marion__spawn",
                    json!({
                        "agent_type": "claude",
                        "prompt": format!("{PUSH_CHILD}: report back through marion."),
                        "timeout_secs": 120,
                        "background": true,
                    }),
                )],
                final_text: "The child is running in the background.".into(),
            },
            NodeScript {
                marker: PUSH_CHILD.into(),
                call_prefix: "tdpushchild".into(),
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
    let run = bed.run(&format!("{PUSH_ROOT}: start one background child."));
    wait_until("the child's first request to be held", || {
        hold.parked.load(Ordering::SeqCst) >= 1
    });
    let root = bed.root().expect("the root's intent is journaled");
    let events = bed.project().agent(&root).events();
    wait_until("the root's first turn to end", || {
        std::fs::read_to_string(&events).is_ok_and(|e| e.contains(r#""type":"result""#))
    });
    hold.release();
    finish(run, RUN_BOUND, &bed.dir.join("run.stderr"));

    let last_root = bed
        .requests()
        .into_iter()
        .rfind(|r| {
            carries(&r["body"], PUSH_ROOT)
                && r["body"]["tools"].as_array().is_some_and(|t| !t.is_empty())
        })
        .expect("the root made requests");
    let body = last_root["body"].to_string();
    assert_eq!(
        body.matches(ENDED).count(),
        1,
        "the model reads the child's end exactly once (request log: {})",
        bed.server.reqlog_path().display()
    );
    assert_eq!(
        delivered(&bed, &root)
            .into_iter()
            .map(|(_, via)| via)
            .collect::<Vec<_>>(),
        ["stream-json:next-turn"],
        "delivered once, as the held parent's next turn"
    );
    assert!(
        bed.records().iter().any(|k| matches!(k,
            RecordKind::MessageQueued(q) if q.agent_id == root)),
        "queued by the supervisor"
    );
}

// ---- the same two properties on a real `opencode acp` root (s36) ---------------------------------

const OC_STEER_ROOT: &str = "MARION-TD-OC-STEER-ROOT-2f77";
const OC_STEER_PREFIX: &str = "tdocsteer";
const OC_STEER_TEXT: &str = "MARION-TD-OC-STEER-TEXT-81ad: also mention the docs";

/// **An operator's steer into a running `opencode acp` node is folded into its running turn.**
/// The ACP row's refinement for opencode is `MidTurn::Fold` (S31 `p0a/acp-opencode-fold`), so marion
/// writes the message as a second `session/prompt` while the first is in flight: written while the
/// turn's second request is held, it is in the third request — after the second call's result,
/// the same turn — and not in the held one, and it is journaled delivered once, `acp:mid-turn`.
#[test]
fn a_steer_into_a_running_acp_opencode_node_is_read_in_the_next_request_of_the_same_turn() {
    let first = format!("{OC_STEER_PREFIX}_00");
    let second = format!("{OC_STEER_PREFIX}_01");
    let (f, s) = (first.clone(), second.clone());
    let hold = Held::on_wire("openai", move |b| {
        carries(b, OC_STEER_ROOT) && carries(b, &f) && !carries(b, &s)
    });
    let Some(bed) = Bed::rooted(
        ("acp-opencode", "120"),
        "opencode",
        "td-oc-steer",
        vec![NodeScript {
            marker: OC_STEER_ROOT.into(),
            call_prefix: OC_STEER_PREFIX.into(),
            turns: vec![
                ScriptedCall::new("marion_list", json!({})),
                ScriptedCall::new("marion_list", json!({})),
            ],
            final_text: "Listed twice. Done.".into(),
        }],
        Arc::clone(&hold),
    ) else {
        return;
    };
    let run = bed.run(&format!("{OC_STEER_ROOT}: list the tree twice."));
    wait_until("the root's second request to be held", || {
        hold.parked.load(Ordering::SeqCst) == 1
    });
    let root = bed.root().expect("the root's intent is journaled");

    let mut client =
        common::client::Client::dial(&common::client::paths_for(&bed.state, &bed.repo));
    let id = client.send(Call::NodeSteer(NodeSteerParams {
        agent_id: root.clone(),
        text: OC_STEER_TEXT.into(),
        caller: None,
    }));
    let (_, outcome) = client.read_to_response(id);
    let Outcome::Result(body) = outcome else {
        panic!("node/steer was refused: {outcome:?}")
    };
    assert_eq!(body["queued"], json!(true), "{body}");
    wait_until("the steer to be delivered mid-turn", || {
        !delivered(&bed, &root).is_empty()
    });
    hold.release();
    finish(run, RUN_BOUND, &bed.dir.join("run.stderr"));

    let root_requests: Vec<Value> = bed
        .requests()
        .into_iter()
        .filter(|r| carries(&r["body"], OC_STEER_ROOT) && carries(&r["body"], &first))
        .collect();
    let held = root_requests
        .iter()
        .find(|r| !carries(&r["body"], &second))
        .expect("the held second request is logged");
    assert!(
        !carries(&held["body"], OC_STEER_TEXT),
        "the steer cannot be in the request that was already in flight"
    );
    let next = root_requests
        .iter()
        .find(|r| carries(&r["body"], &second))
        .expect("the turn went on to a third request");
    assert!(
        carries(&next["body"], OC_STEER_TEXT),
        "opencode folds the second prompt into its running loop: the model reads the steer in \
         the request that carries the second call's result (request log: {})",
        bed.server.reqlog_path().display()
    );
    assert_eq!(
        delivered(&bed, &root)
            .into_iter()
            .map(|(_, via)| via)
            .collect::<Vec<_>>(),
        ["acp:mid-turn"],
        "delivered once, into the running turn"
    );
}

const OC_PUSH_ROOT: &str = "MARION-TD-OC-PUSH-ROOT-6c02";
const OC_PUSH_CHILD: &str = "MARION-TD-OC-PUSH-CHILD-93be";

/// **A background codex child's end reaches its held `opencode acp` parent as the parent's next
/// prompt, once.** The opencode root spawns a codex child in the background and ends its first
/// turn; the child is held at the provider until then, so its end lands while the root is held
/// rather than mid-turn, and marion sends it as the root's second `session/prompt`.
#[test]
fn a_background_codex_childs_end_reaches_its_held_acp_opencode_parent_once_as_its_next_turn() {
    let hold = Held::on_wire("responses", |b| carries(b, OC_PUSH_CHILD));
    let Some(bed) = Bed::rooted(
        ("acp-opencode", "120"),
        "opencode",
        "td-oc-push",
        vec![
            NodeScript {
                marker: OC_PUSH_ROOT.into(),
                call_prefix: "tdocpushroot".into(),
                turns: vec![ScriptedCall::new(
                    "marion_spawn",
                    json!({
                        "agent_type": "codex",
                        "prompt": format!("{OC_PUSH_CHILD}: report back through marion."),
                        "timeout_secs": 120,
                        "background": true,
                    }),
                )],
                final_text: "The child is running in the background.".into(),
            },
            NodeScript {
                marker: OC_PUSH_CHILD.into(),
                call_prefix: "tdocpushchild".into(),
                turns: vec![ScriptedCall::new(
                    "report",
                    json!({"narrative": "Reported from the background."}),
                )],
                final_text: json!({"narrative": "Reported from the background.",
                                   "result_commits": []})
                .to_string(),
            },
        ],
        Arc::clone(&hold),
    ) else {
        return;
    };
    if !marion_testsupport::harness_available("codex") {
        return;
    }
    let run = bed.run(&format!("{OC_PUSH_ROOT}: start one background child."));
    wait_until("the child's first request to be held", || {
        hold.parked.load(Ordering::SeqCst) >= 1
    });
    let root = bed.root().expect("the root's intent is journaled");
    let events = bed.project().agent(&root).events();
    wait_until("the root's first turn to end", || {
        std::fs::read_to_string(&events).is_ok_and(|e| e.contains(r#""stopReason""#))
    });
    hold.release();
    finish(run, RUN_BOUND, &bed.dir.join("run.stderr"));

    let last_root = bed
        .requests()
        .into_iter()
        .rfind(|r| {
            carries(&r["body"], OC_PUSH_ROOT)
                && r["body"]["tools"].as_array().is_some_and(|t| !t.is_empty())
        })
        .expect("the root made requests");
    let body = last_root["body"].to_string();
    assert_eq!(
        body.matches(ENDED).count(),
        1,
        "the model reads the child's end exactly once (request log: {})",
        bed.server.reqlog_path().display()
    );
    assert_eq!(
        delivered(&bed, &root)
            .into_iter()
            .map(|(_, via)| via)
            .collect::<Vec<_>>(),
        ["acp:next-turn"],
        "delivered once, as the held parent's next prompt"
    );
}
