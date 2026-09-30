//! **codex over `codex app-server`, end to end** through marion's supervisor, every model call
//! served by the canned provider (no paid tokens).
//!
//! codex's row selects `Surfaces::AppServer` with its own vocabulary (`marion_harness::codex::APP`),
//! so a codex node takes typed turns over app-server's id-correlated JSON-RPC (S36,
//! `tests/fixtures/app-server-0.155.1/`). Four properties, each read off the provider's own request
//! log, the node's own stream or the journal:
//!
//! 1. an operator's `node/steer` into a running codex **child**, written while one of its requests
//!    is held at the provider, is read in the **next request of the same turn** (P6: `turn/steer`
//!    folds into the running turn), journaled once, `app-server:mid-turn`;
//! 2. a codex child whose wall clock expires with a tool call running is **interrupted**
//!    (`turn/completed` `interrupted`), and marion then kills the turn's processes itself (P7:
//!    app-server leaves the command running), so no process of the tool call survives;
//! 3. a background child that ends while its codex parent is **held** reaches the parent in the
//!    same process as its next turn, once, `app-server:next-turn`;
//! 4. a codex root whose supervisor is SIGKILLed mid-turn is resumed into its own id, and its second
//!    life's `thread/resume` carries the first life's history to the provider (P8).
//!
//! ```sh
//! cargo test -p marion-supervisor --test it_live codex_app_server::
//! ```

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use marion_core::contract::{AgentId, ExitStatus, Isolation, TaskId};
use marion_core::journal::RecordKind;
use marion_core::paths::ProjectDir;
use marion_core::proto::params::NodeSteerParams;
use marion_core::proto::{Call, Outcome};
use marion_provider::{CannedServer, Config, Hold, NodeScript, Script, ScriptedCall};
use marion_supervisor::run::{Caller, Env, SpawnRequest, run_spawn};
use marion_testsupport::{alive, fixture_repo, kill_hard, scratch};
use serde_json::{Value, json};

use crate::common;

/// Generous; it exists so a hang fails instead of wedging the suite.
const RUN_BOUND: Duration = Duration::from_secs(120);

/// codex speaks the OpenAI Responses wire under a canned provider.
const WIRE: &str = "responses";

/// A hold on the Responses requests `pred` picks, released by the test. The request is logged
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
        if wire != Some(WIRE) || !(self.pred)(body) {
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
    fn new(tag: &str, script: Script, hold: Option<Arc<Held>>) -> Option<Bed> {
        if !marion_testsupport::harness_available("codex") {
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
                script,
            },
            hold.map(|h| h as Arc<dyn Hold>),
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

    fn nodes(nodes: Vec<NodeScript>) -> Script {
        Script {
            nodes,
            ..Script::default()
        }
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

    fn marion(&self, args: &[&str], stderr: &str) -> Child {
        Command::new(env!("CARGO_BIN_EXE_marion"))
            // Unsandboxed: its wrapped codex records its runaway pids in the scratch dir, which marion's sandbox refuses.
            .env(marion_harness::os_sandbox::SANDBOX_ENV, "off")
            .args(args)
            .args([
                "--repo",
                &self.repo.to_string_lossy(),
                "--state-dir",
                &self.state.to_string_lossy(),
                "--base-url",
                &self.server.base_url(),
                "--canned",
            ])
            .current_dir(&self.dir)
            .env("PATH", &self.path)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(self.dir.join(stderr)).unwrap())
            .spawn()
            .expect("marion starts")
    }

    fn run(&self, prompt: &str) -> Child {
        self.marion(
            &["run", "codex", "--prompt", prompt, "--timeout", "150"],
            "run.stderr",
        )
    }

    /// What the run left so far, for a failure message: `marion`'s stderr and the provider log.
    fn evidence(&self) -> String {
        let mut out = String::new();
        for f in ["run.stderr", "resume.stderr"] {
            let s = std::fs::read_to_string(self.dir.join(f)).unwrap_or_default();
            out.push_str(&format!("{f}:\n{s}\n"));
        }
        let log = std::fs::read_to_string(self.server.reqlog_path()).unwrap_or_default();
        format!("{out}provider log:\n{log}")
    }

    fn wait(&self, what: &str, mut cond: impl FnMut() -> bool) {
        let until = Instant::now() + RUN_BOUND;
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

/// Wait for a `marion` client to exit inside the bound, killing it on expiry.
fn finish(bed: &Bed, mut run: Child, must_succeed: bool) {
    let until = Instant::now() + RUN_BOUND;
    let status = loop {
        if let Some(s) = run.try_wait().unwrap() {
            break s;
        }
        if Instant::now() >= until {
            let _ = run.kill();
            let _ = run.wait();
            panic!(
                "marion did not finish inside {RUN_BOUND:?}\n{}",
                bed.evidence()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        !must_succeed || status.success(),
        "marion exited {status}\n{}",
        bed.evidence()
    );
}

/// A codex root whose one call is a blocking `spawn` of a codex child with `child_prompt`.
fn delegating_root(marker: &str, child_prompt: String, timeout_secs: u64) -> NodeScript {
    NodeScript {
        marker: marker.into(),
        call_prefix: format!("{}root", marker.to_lowercase().replace('-', "")),
        turns: vec![ScriptedCall::new(
            "spawn",
            json!({
                "agent_type": "codex",
                "prompt": child_prompt,
                "acceptance_criteria": [],
                "timeout_secs": timeout_secs,
            }),
        )],
        final_text: "The child is done.".into(),
    }
}

const STEER_ROOT: &str = "MARION-CX-STEER-ROOT-71d0";
const STEER_CHILD: &str = "MARION-CX-STEER-CHILD-c24e";
const STEER_PREFIX: &str = "cxsteer";
const STEER_TEXT: &str = "MARION-CX-STEER-TEXT-9a13: also mention the docs";

/// **An operator's steer into a running codex child is folded into its running turn**: written
/// while the child's second request is held, it is in the third request — after the second call's
/// result, in the same turn — and not in the held one.
#[test]
fn a_steer_into_a_running_codex_child_is_read_in_the_next_request_of_the_same_turn() {
    let first = format!("{STEER_PREFIX}_00");
    let second = format!("{STEER_PREFIX}_01");
    let (f, s) = (first.clone(), second.clone());
    let hold = Held::on(move |b| {
        carries(b, STEER_CHILD) && !carries(b, STEER_ROOT) && carries(b, &f) && !carries(b, &s)
    });
    let Some(bed) = Bed::new(
        "cx-steer",
        Bed::nodes(vec![
            delegating_root(
                STEER_ROOT,
                format!("{STEER_CHILD}: list the tree twice."),
                120,
            ),
            NodeScript {
                marker: STEER_CHILD.into(),
                call_prefix: STEER_PREFIX.into(),
                turns: vec![
                    ScriptedCall::new("list", json!({})),
                    ScriptedCall::new("list", json!({})),
                ],
                final_text: "Listed twice. Done.".into(),
            },
        ]),
        Some(Arc::clone(&hold)),
    ) else {
        return;
    };
    let run = bed.run(&format!("{STEER_ROOT}: delegate one child."));
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
    finish(&bed, run, true);

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
    // The steer, once, into the running turn; then marion's one request for the report the
    // child's script never makes (§7.6's grace turn), as its next turn.
    assert_eq!(
        bed.delivered(&child),
        ["app-server:mid-turn", "app-server:next-turn"],
        "the steer delivered once, into the running turn, then the report request"
    );
    let events = bed.events_of(&child);
    // Two turns: the first, which took the steer, and marion's report request. The steer opened
    // none of its own.
    assert_eq!(
        events.matches(r#""method":"turn/started""#).count(),
        2,
        "the steer opened no turn of its own:\n{events}"
    );
}

/// The child's bound. Long enough for codex to boot, take turn one and get its tool call running;
/// short enough that the test is not a wait.
const TIMEOUT_CHILD_SECS: u64 = 25;

/// How long the runaway `sleep`s live if nothing kills them: far past this test.
const RUNAWAY_SECS: u64 = 900;

/// `timeout_kill.rs`'s case B: a shell that backgrounds one sleeper and `exec`s into another,
/// recording both pids, so the tool call is still running when the bound expires.
fn runaway_script(path: &Path, pidfile: &Path) {
    std::fs::write(
        path,
        format!(
            "#!/bin/sh\n\
             : > '{pidfile}'\n\
             /bin/sleep {RUNAWAY_SECS} &\n\
             echo \"$!\" >> '{pidfile}'\n\
             echo \"$$\" >> '{pidfile}'\n\
             exec /bin/sleep {RUNAWAY_SECS}\n",
            pidfile = pidfile.display()
        ),
    )
    .expect("runaway script is written");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn case_b_js(runaway: &Path, pidfile: &Path) -> String {
    let cmd = serde_json::to_string(&format!(
        "/bin/sh {} {}",
        runaway.display(),
        pidfile.display()
    ))
    .unwrap();
    let hold = serde_json::to_string(&format!("/bin/sleep {RUNAWAY_SECS}")).unwrap();
    format!(
        "// @exec: {{\"yield_time_ms\": 900000, \"max_output_tokens\": 2000}}\n\
         const b = await tools.exec_command({{ cmd: {cmd}, shell: \"/bin/sh\", login: false, \
         yield_time_ms: 250, max_output_tokens: 2000 }});\n\
         text(JSON.stringify({{caseB: b}}));\n\
         const held = await tools.exec_command({{ cmd: {hold}, shell: \"/bin/sh\", login: false, \
         yield_time_ms: 900000, max_output_tokens: 200 }});\n\
         text(JSON.stringify({{held: held}}));\n"
    )
}

fn pids_in(path: &Path) -> Vec<i32> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// **A codex child past its wall clock is interrupted, and no process of its tool call survives.**
/// app-server answers `turn/interrupt` at once and leaves the running command alive (P7), so what
/// is asserted is marion's own kill of the turn's processes after the interrupt.
#[test]
fn a_timed_out_codex_child_is_interrupted_and_leaves_no_process_of_its_turn() {
    if !marion_testsupport::harness_available("codex") {
        return;
    }
    let dir = scratch("cx-interrupt");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let pidfile = dir.join("runaway-pids.txt");
    let runaway = dir.join("runaway.sh");
    runaway_script(&runaway, &pidfile);
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: dir.join("provider-requests.jsonl"),
        script: Script {
            child_exec_js: Some(case_b_js(&runaway, &pidfile)),
            ..Script::default()
        },
    })
    .expect("the canned provider binds");
    let project = ProjectDir::new(&state, &repo);
    let env = Env {
        // Unsandboxed: the tool call records its runaway pids in the scratch dir, outside the
        // node's workspace, which marion's sandbox refuses.
        os_sandbox: false,
        project_dir: project.clone(),
        project_root: repo.clone(),
        state: state.clone(),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url: Some(server.base_url()),
        auth: marion_harness::Auth::Canned,
    };
    let req = SpawnRequest {
        review: None,
        race: None,
        agent_type: "codex".into(),
        prompt: "Start the long-running command and keep it running.".into(),
        repo: repo.clone(),
        acceptance_criteria: vec!["the command is running".into()],
        verification: vec![],
        writable_scope: vec!["src/**".into()],
        timeout_secs: TIMEOUT_CHILD_SECS,
        model: None,
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
        profile: None,
        budget: None,
        read_only: false,
        workflow: None,
    };
    let caller = Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    );
    let started = Instant::now();
    let contract = run_spawn(&env, &req, &TaskId("cx-interrupt".into()), &caller)
        .expect("the spawn path runs to a contract");
    let elapsed = started.elapsed();

    // Give killed processes a moment to leave the table, then clean up unconditionally — before a
    // single assertion — so a failing run can never be the leak it is testing for.
    let recorded = pids_in(&pidfile);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut survivors = recorded.clone();
    while Instant::now() < deadline {
        survivors.retain(|p| alive(*p));
        if survivors.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    for p in &recorded {
        kill_hard(*p);
    }
    drop(server);

    assert_eq!(
        recorded.len(),
        2,
        "the tool call never recorded its runaway pids within {elapsed:?}, so the run never \
         reached a running command and the criterion would be vacuous: {recorded:?}"
    );
    assert!(
        survivors.is_empty(),
        "processes of the interrupted turn outlived the node: {survivors:?}"
    );
    let completion = contract
        .completion
        .as_ref()
        .expect("a terminal node has a completion");
    assert_eq!(
        completion.status,
        ExitStatus::TimedOut,
        "ran {elapsed:?} against a {TIMEOUT_CHILD_SECS}s bound: {}",
        completion.exit.description
    );
    let child = common::journal::records(&project.journal())
        .into_iter()
        .find_map(|k| match k {
            RecordKind::SpawnIntent(i) => Some(i.agent_id),
            _ => None,
        })
        .expect("the child's intent is journaled");
    let events = std::fs::read_to_string(project.agent(&child).events()).unwrap_or_default();
    assert!(
        events.contains(r#""status":"interrupted""#),
        "marion interrupted the turn over app-server before killing it:\n{events}"
    );
}

const PUSH_ROOT: &str = "MARION-CX-PUSH-ROOT-3e91";
const PUSH_CHILD: &str = "MARION-CX-PUSH-CHILD-b0a7";
const ENDED: &str = "you backgrounded as task_id";

/// **A background child's end reaches its held codex parent in the same process as its next
/// turn, once.** The child is held at the provider until the parent's first turn is over, so the
/// end lands while the parent is held rather than mid-turn.
#[test]
fn a_background_childs_end_reaches_its_held_codex_parent_once_as_its_next_turn() {
    let hold = Held::on(|b| carries(b, PUSH_CHILD) && !carries(b, PUSH_ROOT));
    let Some(bed) = Bed::new(
        "cx-push",
        Bed::nodes(vec![
            NodeScript {
                marker: PUSH_ROOT.into(),
                call_prefix: "cxpushroot".into(),
                turns: vec![ScriptedCall::new(
                    "spawn",
                    json!({
                        "agent_type": "codex",
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
                call_prefix: "cxpushchild".into(),
                turns: vec![ScriptedCall::new(
                    "report",
                    json!({"narrative": "Reported from the background."}),
                )],
                final_text: "Reported.".into(),
            },
        ]),
        Some(Arc::clone(&hold)),
    ) else {
        return;
    };
    let run = bed.run(&format!("{PUSH_ROOT}: start one background child."));
    bed.wait("the child's first request to be held", || {
        hold.parked.load(Ordering::SeqCst) >= 1
    });
    let root = bed.root().expect("the root's intent is journaled");
    bed.wait("the root's first turn to end", || {
        bed.events_of(&root)
            .contains(r#""method":"turn/completed""#)
    });
    hold.release();
    finish(&bed, run, true);

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
        ["app-server:next-turn"],
        "delivered once, as the held parent's next turn in the same process"
    );
    let events = bed.events_of(&root);
    assert_eq!(
        events.matches(r#""method":"turn/started""#).count(),
        2,
        "two turns of one codex process:\n{events}"
    );
    let spawned = bed
        .records()
        .into_iter()
        .filter(|k| matches!(k, RecordKind::Spawned(s) if s.agent_id == root))
        .count();
    assert_eq!(spawned, 1, "one process, not a relaunch");
}

const RESUME_ROOT: &str = "MARION-CX-RESUME-ROOT-58ac";
const RESUME_PROMPT: &str = "MARION-CX-RESUME-PROMPT-d6f2: continue and finish.";

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// The supervisor that wrote the journal's latest line: its `writer` is `<pid>-…`.
fn supervisor_pid(bed: &Bed) -> Option<i32> {
    let bytes = std::fs::read(bed.project().journal()).unwrap_or_default();
    let last = String::from_utf8_lossy(&bytes).lines().last()?.to_string();
    let v: Value = serde_json::from_str(&last).ok()?;
    v["writer"]
        .as_str()
        .and_then(|w| w.split('-').next())
        .and_then(|p| p.parse().ok())
}

/// **A codex root resumes over app-server after its supervisor is SIGKILLed.** Parked mid-turn on
/// a held request with its thread journaled, the supervisor dies uncatchably; `marion resume`
/// relaunches the root into its own id, and the second life's `thread/resume` hands codex its
/// history, so the first request after the relaunch carries both the resume prompt and the first
/// life's task.
#[test]
fn a_codex_root_resumes_its_thread_after_its_supervisor_is_sigkilled() {
    let hold = Held::on(|b| carries(b, RESUME_ROOT) && !carries(b, RESUME_PROMPT));
    let Some(bed) = Bed::new(
        "cx-resume",
        Bed::nodes(vec![NodeScript {
            marker: RESUME_ROOT.into(),
            call_prefix: "cxresumeroot".into(),
            turns: vec![ScriptedCall::new("list", json!({}))],
            final_text: "Listed. Done.".into(),
        }]),
        Some(Arc::clone(&hold)),
    ) else {
        return;
    };
    let mut run = bed.run(&format!("{RESUME_ROOT}: list the tree."));
    bed.wait("the root's first request to be held", || {
        hold.parked.load(Ordering::SeqCst) >= 1
    });
    let root = bed.root().expect("the root's intent is journaled");
    bed.wait("the root's thread to be journaled", || {
        marion_core::registry::replay(&std::fs::read(bed.project().journal()).unwrap_or_default())
            .nodes()
            .iter()
            .any(|n| n.agent_id == root && n.harness_session.is_some())
    });

    let sup = supervisor_pid(&bed).expect("the journal names its supervisor");
    unsafe { kill(sup, 9) };
    bed.wait("the supervisor to be gone", || {
        !marion_testsupport::alive(sup)
    });
    let _ = run.kill();
    let _ = run.wait();
    let seq_before = bed
        .requests()
        .iter()
        .filter_map(|r| r["seq"].as_u64())
        .max()
        .unwrap_or(0);
    // The first life's held request is released with its process already gone.
    hold.release();

    let resume = bed.marion(
        &["resume", &root.0, "--prompt", RESUME_PROMPT],
        "resume.stderr",
    );
    finish(&bed, resume, false);

    let second: Vec<Value> = bed
        .requests()
        .into_iter()
        .filter(|r| r["seq"].as_u64().is_some_and(|s| s > seq_before))
        .collect();
    assert!(
        second
            .iter()
            .any(|r| carries(&r["body"], RESUME_PROMPT) && carries(&r["body"], RESUME_ROOT)),
        "the second life's request carries the resume prompt and the first life's task — \
         thread/resume replayed the thread\n{}",
        bed.evidence()
    );
    let events = bed.events_of(&root);
    assert!(
        events.contains(r#""method":"turn/completed""#),
        "the second life ran over app-server:\n{events}"
    );
    let spawned = bed
        .records()
        .into_iter()
        .filter(|k| matches!(k, RecordKind::Spawned(s) if s.agent_id == root))
        .count();
    assert_eq!(spawned, 2, "two lives of one node id");
    if let Some(pid) = supervisor_pid(&bed) {
        unsafe { kill(pid, 9) };
    }
}
