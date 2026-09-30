//! **Workflows, end to end: real claude nodes against marion's canned provider.**
//!
//! Each test writes a workflow into the bed's own user configuration (or, for the trust test, into
//! the repository), starts it over the socket with `workflow/run`, and waits for the run to close
//! by the badge its step nodes carry. What is asserted is what the run did: which steps ran and how
//! each ended, what a later step's prompt was given of an earlier step's output, and that nothing is
//! journaled for a workflow marion refuses.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use marion_core::journal::RecordKind;
use marion_core::paths::ProjectDir;
use marion_core::proto::notify::Event as Note;
use marion_core::proto::params::WorkflowRunParams;
use marion_core::proto::{Call, Method, MethodResult};
use marion_core::workflow::{Outcome as RunOutcome, StepVerdict, WorkflowId};
use marion_provider::{CannedServer, Config, Hold, NodeScript, Script, ScriptedCall};
use marion_supervisor::socket::project_root;
use marion_testsupport::{fixture_repo, scratch};
use serde_json::json;

mod common;
use common::client::{Client, paths_for};
use common::journal::records;

const BOUND: Duration = Duration::from_secs(240);

/// A claude node, keyed on `marker` in its request, that runs `turns` (each a `Write` of a file)
/// and then reports `narrative`.
fn node(marker: &str, writes: &[&str], narrative: &str) -> NodeScript {
    let claude = marion_harness::adapter_for(marion_core::harness::Harness::ClaudeCode)
        .expect("claude has an adapter");
    let mut turns: Vec<ScriptedCall> = writes
        .iter()
        .map(|f| {
            ScriptedCall::new(
                "Write",
                json!({"file_path": f, "content": format!("work by {marker}\n")}),
            )
        })
        .collect();
    turns.push(ScriptedCall::new(
        claude.marion_tool_name("report"),
        json!({"narrative": narrative}),
    ));
    NodeScript {
        marker: marker.into(),
        call_prefix: marker.to_lowercase(),
        turns,
        final_text: "Done.".into(),
    }
}

/// The canned provider, a supervisor whose configuration and trust store are the bed's own, and a
/// repository. `None` where claude is not installed.
struct Bed {
    repo: PathBuf,
    state: PathBuf,
    config: PathBuf,
    data: PathBuf,
    project: ProjectDir,
    reqlog: PathBuf,
    sup: common::Supervisor,
    _server: CannedServer,
    _dir: marion_testsupport::Scratch,
}

impl Drop for Bed {
    fn drop(&mut self) {
        self.sup.stop();
    }
}

fn bed(tag: &str, script: Script) -> Option<Bed> {
    bed_held(tag, script, None)
}

/// A bed whose provider consults `hold` before each answer.
fn bed_held(tag: &str, script: Script, hold: Option<Arc<dyn Hold>>) -> Option<Bed> {
    if !marion_testsupport::harness_available("claude") {
        return None;
    }
    let dir = scratch(tag);
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    let config = dir.join("config");
    let data = dir.join("data");
    for d in [&state, &config, &data] {
        std::fs::create_dir_all(d).unwrap();
    }
    let key = project_root(&repo);
    let reqlog = dir.join("provider-requests.jsonl");
    let server = CannedServer::start_held(
        Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog: reqlog.clone(),
            script,
        },
        hold,
    )
    .expect("the canned provider binds");
    let sup = common::Supervisor::start_with(
        &state,
        &key,
        &std::env::var("PATH").unwrap_or_default(),
        &server.base_url(),
        BOUND,
        &[("XDG_CONFIG_HOME", &config), ("XDG_DATA_HOME", &data)],
    );
    Some(Bed {
        project: ProjectDir::new(&state, &key),
        repo,
        state,
        config,
        data,
        reqlog,
        sup,
        _server: server,
        _dir: dir,
    })
}

impl Bed {
    /// `text` as the operator's own workflow `name`.
    fn user_workflow(&self, name: &str, text: &str) {
        let dir = self.config.join("marion").join("workflows");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{name}.toml")), text).unwrap();
    }

    /// Start `name` over the socket; the run's id, or the supervisor's refusal.
    fn run(&self, name: &str, inputs: &[(&str, &str)]) -> Result<WorkflowId, String> {
        let body = self.sup.call(Call::WorkflowRun(WorkflowRunParams {
            name: name.into(),
            inputs: inputs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            repo: self.repo.clone(),
        }))?;
        let MethodResult::WorkflowRun(r) = Method::WorkflowRun.decode_result(&body).unwrap() else {
            panic!("wrong result")
        };
        Ok(r.wf_id)
    }

    /// Start `name` and wait for the run to close: its result.
    fn run_to_close(
        &self,
        name: &str,
        inputs: &[(&str, &str)],
    ) -> marion_core::workflow::WorkflowResult {
        let mut watcher = self.watcher();
        let wf = self.run(name, inputs).expect("the run opens");
        self.await_close(&mut watcher, &wf)
    }

    /// A client subscribed to the tree, to watch for a run's close.
    fn watcher(&self) -> Client {
        let mut watcher = Client::dial(&paths_for(&self.state, &self.repo));
        watcher.read_bound(BOUND);
        watcher.tree();
        watcher
    }

    /// Wait for run `wf` to close: its result.
    fn await_close(
        &self,
        watcher: &mut Client,
        wf: &WorkflowId,
    ) -> marion_core::workflow::WorkflowResult {
        let wf = wf.clone();
        let closed = |n: &marion_core::proto::model::NodeSummary| {
            n.workflow
                .as_ref()
                .is_some_and(|b| b.wf_id == wf && b.closed.is_some())
        };
        if marion_supervisor::workflow::read_result(&self.project, &wf).is_none() {
            let waited = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                watcher
                    .wait_for_event(|n| matches!(n, Note::NodeAdded { node, .. } if closed(node)))
            }));
            if waited.is_err() {
                let kinds: Vec<String> = records(&self.project.journal())
                    .iter()
                    .map(|k| format!("{k:?}").chars().take(200).collect())
                    .collect();
                panic!("the run never closed; journal:\n{}", kinds.join("\n"));
            }
        }
        marion_supervisor::workflow::read_result(&self.project, &wf)
            .expect("the result is on disk once the close is announced")
    }

    /// `marion workflow <args>` against the bed: its exit code and stdout.
    fn cli(&self, args: &[&str]) -> (Option<i32>, String) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_marion"));
        cmd.arg("workflow")
            .args(args)
            .arg("--repo")
            .arg(&self.repo)
            .arg("--state-dir")
            .arg(&self.state);
        for (k, v) in [
            ("XDG_CONFIG_HOME", &self.config),
            ("XDG_DATA_HOME", &self.data),
        ] {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("marion runs");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code(), text)
    }

    /// The provider requests whose body contains `marker`.
    fn requests_with(&self, marker: &str) -> Vec<String> {
        std::fs::read_to_string(&self.reqlog)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains(marker))
            .map(str::to_string)
            .collect()
    }
}

fn verdicts(r: &marion_core::workflow::WorkflowResult) -> Vec<Option<StepVerdict>> {
    r.steps.iter().map(|s| s.verdict).collect()
}

/// **(a) A plan's report reaches the next step's prompt**, fenced as another agent's output, and a
/// two-step run of a read-only planner and a writer succeeds with each step's own verdict. The
/// run is opened before its first step's intent, and every step node's intent names its seat.
#[test]
fn a_plan_steps_report_reaches_the_next_steps_prompt() {
    let script = Script {
        nodes: vec![
            node("PLANMARK", &[], "the plan is PLAN-7f3: add ok"),
            node("IMPLMARK", &["ok"], "wrote ok"),
        ],
        ..Script::default()
    };
    let Some(bed) = bed("wf-plan", script) else {
        return;
    };
    bed.user_workflow(
        "ship",
        r#"schema = 1
name = "ship"
inputs = ["task"]

[[step]]
id = "plan"
kind = "agent"
on = "claude"
read_only = true
prompt = "PLANMARK: plan {input.task}"

[[step]]
id = "impl"
kind = "agent"
on = "claude"
prompt = "IMPLMARK: do it.\nPlan:\n{plan.report}"
verify = ["test -f ok"]
"#,
    );
    let result = bed.run_to_close("ship", &[("task", "the ok file")]);
    assert_eq!(
        result.outcome,
        RunOutcome::Succeeded,
        "{}",
        result.scoreboard()
    );
    assert_eq!(
        verdicts(&result),
        [Some(StepVerdict::Succeeded), Some(StepVerdict::Succeeded)]
    );
    let impl_requests = bed.requests_with("IMPLMARK");
    assert!(!impl_requests.is_empty(), "the second step ran");
    assert!(
        impl_requests[0].contains("PLAN-7f3") && impl_requests[0].contains("plan.report"),
        "the plan's report reached the next prompt, fenced: {}",
        impl_requests[0].chars().take(600).collect::<String>()
    );
    assert!(
        bed.requests_with("PLANMARK")[0].contains("the ok file"),
        "the input reached the first prompt"
    );

    let journal = records(&bed.project.journal());
    let opened = journal
        .iter()
        .position(|k| matches!(k, RecordKind::WorkflowOpened(_)))
        .expect("the run is opened");
    let first = journal
        .iter()
        .position(|k| matches!(k, RecordKind::SpawnIntent(i) if i.workflow.is_some()))
        .expect("a step node's intent");
    assert!(opened < first, "opened before the first step's intent");
    let seats: Vec<u8> = journal
        .iter()
        .filter_map(|k| match k {
            RecordKind::SpawnIntent(i) => i.workflow.as_ref().map(|s| s.step),
            _ => None,
        })
        .collect();
    assert_eq!(seats, [0, 1], "one node per step, each naming its step");
    assert!(
        journal
            .iter()
            .any(|k| matches!(k, RecordKind::WorkflowClosed(_)))
    );
    assert!(
        !bed.repo.join("ok").exists(),
        "the work is on a branch, not in the checkout"
    );
}

/// **(e) `when` escalates both ways**: a step that fails runs the step gated on its failure and
/// skips the one gated on its success, and the run succeeds because the failure was handled.
#[test]
fn when_runs_the_escalation_and_skips_the_other_branch() {
    let script = Script {
        nodes: vec![
            node("TRYMARK", &["wrong.txt"], "tried"),
            node("RESCUEMARK", &["ok"], "rescued"),
            node("NEVERMARK", &["never.txt"], "should not run"),
        ],
        ..Script::default()
    };
    let Some(bed) = bed("wf-when", script) else {
        return;
    };
    bed.user_workflow(
        "escalate",
        r#"schema = 1
name = "escalate"

[[step]]
id = "try"
kind = "agent"
on = "claude"
prompt = "TRYMARK: make ok"
verify = ["test -f ok"]

[[step]]
id = "rescue"
kind = "agent"
on = "claude"
when = "try:failed"
prompt = "RESCUEMARK: make ok properly"
verify = ["test -f ok"]

[[step]]
id = "celebrate"
kind = "agent"
on = "claude"
when = "try:succeeded"
prompt = "NEVERMARK"
"#,
    );
    let result = bed.run_to_close("escalate", &[]);
    assert_eq!(
        verdicts(&result),
        [
            Some(StepVerdict::Failed),
            Some(StepVerdict::Succeeded),
            Some(StepVerdict::Skipped)
        ],
        "{}",
        result.scoreboard()
    );
    assert_eq!(result.outcome, RunOutcome::Succeeded);
    assert!(
        bed.requests_with("NEVERMARK").is_empty(),
        "the other branch never ran"
    );
}

/// **(k) A parallel step fans out and fans in**: three read-only agents run the same task, and the
/// next step's prompt is given all three reports.
#[test]
fn a_parallel_step_fans_in_every_report() {
    let script = Script {
        nodes: vec![
            node("fan-model-one", &[], "view ONE-a1"),
            node("fan-model-two", &[], "view TWO-b2"),
            node("fan-model-three", &[], "view THREE-c3"),
            node("SUMMARK", &[], "summed"),
        ],
        ..Script::default()
    };
    let Some(bed) = bed("wf-parallel", script) else {
        return;
    };
    bed.user_workflow(
        "survey",
        r#"schema = 1
name = "survey"

[[step]]
id = "fan"
kind = "parallel"
on = ["claude:fan-model-one", "claude:fan-model-two", "claude:fan-model-three"]
prompt = "Look at the repository and say what you see."

[[step]]
id = "sum"
kind = "agent"
on = "claude"
read_only = true
prompt = "SUMMARK: combine\n{fan.report}"
"#,
    );
    let result = bed.run_to_close("survey", &[]);
    assert_eq!(
        result.outcome,
        RunOutcome::Succeeded,
        "{}",
        result.scoreboard()
    );
    assert_eq!(
        result.steps[0].nodes.len(),
        3,
        "three nodes ran the fan-out"
    );
    let sum = bed.requests_with("SUMMARK");
    assert!(!sum.is_empty());
    for view in ["ONE-a1", "TWO-b2", "THREE-c3"] {
        assert!(sum[0].contains(view), "{view} reached the fan-in");
    }
}

/// **(j) A repository's workflow runs only once trusted**: refused by name before anything is
/// journaled, and a missing input is refused the same way.
#[test]
fn an_untrusted_repository_workflow_is_refused_before_anything_is_journaled() {
    let Some(bed) = bed("wf-trust", Script::default()) else {
        return;
    };
    let dir = bed.repo.join(".marion").join("workflows");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("repo-flow.toml"),
        "schema = 1\nname = \"repo-flow\"\n[[step]]\nid = \"a\"\nkind = \"agent\"\non = \"claude\"\nprompt = \"p\"\n",
    )
    .unwrap();
    let e = bed
        .run("repo-flow", &[])
        .expect_err("an untrusted repository file");
    assert!(e.contains("marion trust allow"), "{e}");
    bed.user_workflow(
        "needs",
        "schema = 1\nname = \"needs\"\ninputs = [\"task\"]\n[[step]]\nid = \"a\"\nkind = \"agent\"\non = \"claude\"\nprompt = \"{input.task}\"\n",
    );
    let e = bed.run("needs", &[]).expect_err("a missing input");
    assert!(e.contains("task"), "{e}");
    let e = bed.run("nosuch", &[]).expect_err("no such workflow");
    assert!(e.contains("no workflow named"), "{e}");
    assert!(
        !records(&bed.project.journal())
            .iter()
            .any(|k| matches!(k, RecordKind::WorkflowOpened(_))),
        "nothing refused was journaled"
    );
}

/// **`marion workflow run` from a terminal**: the run is started, watched to its close, its
/// scoreboard printed, and the exit code is its outcome.
#[test]
fn marion_workflow_run_prints_the_scoreboard_and_exits_with_the_outcome() {
    let script = Script {
        nodes: vec![node("CLIMARK", &["ok"], "wrote ok")],
        ..Script::default()
    };
    let Some(bed) = bed("wf-cli", script) else {
        return;
    };
    bed.user_workflow(
        "one",
        "schema = 1\nname = \"one\"\ninputs = [\"task\"]\n[[step]]\nid = \"do\"\nkind = \"agent\"\non = \"claude\"\nprompt = \"CLIMARK {input.task}\"\nverify = [\"test -f ok\"]\n",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args(["workflow", "run", "one", "--input", "task=make ok"])
        .arg("--repo")
        .arg(&bed.repo)
        .arg("--state-dir")
        .arg(&bed.state)
        .env("XDG_CONFIG_HOME", &bed.config)
        .env("XDG_DATA_HOME", &bed.data)
        .output()
        .expect("marion runs");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("do") && stdout.contains("succeeded"),
        "{stdout}"
    );
}

/// **(b) A race step decides its winner and the next step builds on the winner's work**: three
/// seats, seat two writes the file the verification wants and wins; the next step's worktree is cut
/// at the winner's commit, so its own verification finds the file without writing it, and its
/// prompt names the winner's branch.
#[test]
fn a_race_steps_winner_is_what_the_next_step_builds_on() {
    let script = Script {
        nodes: vec![
            node("wf-race-one", &["wrong-1.txt"], "seat one"),
            node("wf-race-two", &["ok"], "seat two wrote ok"),
            node("wf-race-three", &["wrong-3.txt"], "seat three"),
            node("NEXTMARK", &[], "checked"),
        ],
        ..Script::default()
    };
    let Some(bed) = bed("wf-race", script) else {
        return;
    };
    bed.user_workflow(
        "raced",
        r#"schema = 1
name = "raced"

[[step]]
id = "impl"
kind = "race"
on = ["claude:wf-race-one", "claude:wf-race-two", "claude:wf-race-three"]
prompt = "Create the file the task needs at the repository root."
verify = ["test -f ok"]

[[step]]
id = "next"
kind = "agent"
on = "claude"
prompt = "NEXTMARK: build on {impl.branch}"
verify = ["test -f ok"]
"#,
    );
    let result = bed.run_to_close("raced", &[]);
    assert_eq!(
        result.outcome,
        RunOutcome::Succeeded,
        "{}",
        result.scoreboard()
    );
    assert_eq!(result.steps[0].nodes.len(), 3, "three seats");
    let winner_branch = result.steps[0].branch.clone().expect("the winner's branch");
    let next = bed.requests_with("NEXTMARK");
    assert!(
        next[0].contains(&winner_branch),
        "the next prompt names the winner's branch {winner_branch}"
    );
    let journal = records(&bed.project.journal());
    assert!(
        journal
            .iter()
            .any(|k| matches!(k, RecordKind::RaceDecided(_)))
    );
    let seats = journal
        .iter()
        .filter(|k| matches!(k, RecordKind::SpawnIntent(i) if i.race.is_some() && i.workflow.as_ref().is_some_and(|s| s.step == 0)))
        .count();
    assert_eq!(seats, 3, "each seat carries its race and its workflow step");
}

const REVIEWED_FILE: &str = "src/feature.txt";

/// A reviewer's report as JSON: `block` with one high finding on the reviewed file, or `allow`.
fn reviewer(marker: &str, block: bool) -> NodeScript {
    let report = if block {
        json!({
            "verdict": "block",
            "summary": "one problem",
            "findings": [{
                "severity": "high",
                "file": REVIEWED_FILE,
                "line": 1,
                "claim": "the feature line is wrong",
                "evidence": "line 1",
                "recommendation": "write the right line"
            }]
        })
    } else {
        json!({"verdict": "allow", "summary": "fine", "findings": []})
    };
    node(marker, &[], &report.to_string())
}

/// The work, a reviewer that blocks it, a fixer, and a reviewer of the fix. Listed so the first
/// marker a request carries is its own: a later request replays earlier task text.
fn review_script(fix_passes: bool) -> Script {
    Script {
        nodes: vec![
            // A review prompt quotes the reviewed node's report after "its report: ", which the
            // reviewed node's own later turns never carry.
            reviewer("its report: FIXED-9x", !fix_passes),
            node("found blocking problems", &[REVIEWED_FILE], "FIXED-9x"),
            reviewer("its report: WORK-1a", true),
            node("WORKMARK", &[REVIEWED_FILE], "WORK-1a"),
            node("AFTERMARK", &[], "noted"),
        ],
        ..Script::default()
    }
}

fn reviewed_workflow(max_rounds: u8) -> String {
    format!(
        r#"schema = 1
name = "reviewed"

[[step]]
id = "work"
kind = "agent"
on = "claude"
prompt = "WORKMARK: write {REVIEWED_FILE}"

[[step]]
id = "gate"
kind = "review"
of = "work"
on = "claude"
max_rounds = {max_rounds}

[[step]]
id = "after"
kind = "agent"
on = "claude"
read_only = true
prompt = "AFTERMARK: the review said\n{{gate.findings}}\non {{gate.branch}}"
"#
    )
}

/// **(c) A review blocks, the work is fixed, the fix is reviewed clean**, and the run goes on to
/// the next step on the fixed branch: two review rounds, one fixer cut at the work's commit and
/// told the grounded findings.
#[test]
fn a_blocking_review_is_fixed_and_reviewed_clean() {
    let Some(bed) = bed("wf-review", review_script(true)) else {
        return;
    };
    bed.user_workflow("reviewed", &reviewed_workflow(2));
    let result = bed.run_to_close("reviewed", &[]);
    assert_eq!(
        result.outcome,
        RunOutcome::Succeeded,
        "{}",
        result.scoreboard()
    );
    assert_eq!(
        verdicts(&result),
        [
            Some(StepVerdict::Succeeded),
            Some(StepVerdict::Clean),
            Some(StepVerdict::Succeeded)
        ]
    );
    assert_eq!(result.steps[1].nodes.len(), 3, "reviewer, fixer, reviewer");
    let fix = bed.requests_with("found blocking problems");
    assert!(
        fix.iter().any(|r| r.contains("the feature line is wrong")),
        "the fixer was told the grounded finding"
    );
    let after = bed.requests_with("AFTERMARK");
    let fixed_branch = result.steps[1].branch.clone().expect("the fixed branch");
    assert!(
        after[0].contains(&fixed_branch),
        "the next step names the fixed branch"
    );
}

/// **(d) A review still blocking at its last round fails the run**, and `marion workflow run` says
/// so with exit 1; nothing after it runs.
#[test]
fn a_review_still_blocking_at_its_last_round_fails_the_run() {
    let Some(bed) = bed("wf-review-fail", review_script(false)) else {
        return;
    };
    bed.user_workflow("reviewed", &reviewed_workflow(2));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_marion"));
    cmd.args(["workflow", "run", "reviewed"])
        .arg("--repo")
        .arg(&bed.repo)
        .arg("--state-dir")
        .arg(&bed.state);
    for (k, v) in [
        ("XDG_CONFIG_HOME", &bed.config),
        ("XDG_DATA_HOME", &bed.data),
    ] {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("marion runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.contains("blocked") && stdout.contains("failed"),
        "{stdout}"
    );
    assert!(
        bed.requests_with("AFTERMARK").is_empty(),
        "nothing ran after the blocked review"
    );
}

/// **A hold on every request that carries `marker`**, until released: the nodes it catches have
/// asked their question and wait for an answer, so a test can act on them mid-turn.
#[derive(Debug)]
struct MarkerHold {
    marker: String,
    parked: AtomicU64,
    released: Mutex<bool>,
    wake: Condvar,
}

impl MarkerHold {
    fn new(marker: &str) -> Arc<MarkerHold> {
        Arc::new(MarkerHold {
            marker: marker.into(),
            parked: AtomicU64::new(0),
            released: Mutex::new(false),
            wake: Condvar::new(),
        })
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }

    /// Wait, bounded, until `n` requests are held.
    fn await_parked(&self, n: u64) {
        let until = Instant::now() + BOUND;
        while self.parked.load(Ordering::SeqCst) < n {
            assert!(Instant::now() < until, "{n} requests were never held");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Hold for MarkerHold {
    fn wait_for(&self, wire: Option<&str>, body: &serde_json::Value) {
        if wire != Some("anthropic") || !body.to_string().contains(&self.marker) {
            return;
        }
        let mut released = self.released.lock().unwrap();
        self.parked.fetch_add(1, Ordering::SeqCst);
        while !*released {
            released = self.wake.wait(released).unwrap();
        }
        self.parked.fetch_sub(1, Ordering::SeqCst);
    }
}

/// **(h) `marion workflow cancel` during a race** cancels every running seat, as the workflow's
/// cancel, launches nothing more, and closes the run cancelled; a closed run cannot be cancelled
/// again.
#[test]
fn cancelling_a_run_during_its_race_stops_the_seats_and_closes_it_cancelled() {
    let script = Script {
        nodes: vec![
            node("wf-hold-one", &["one.txt"], "seat one"),
            node("wf-hold-two", &["two.txt"], "seat two"),
            node("AFTERMARK", &[], "after"),
        ],
        ..Script::default()
    };
    let hold = MarkerHold::new("wf-hold-");
    let Some(bed) = bed_held("wf-cancel", script, Some(hold.clone())) else {
        return;
    };
    bed.user_workflow(
        "held",
        r#"schema = 1
name = "held"

[[step]]
id = "impl"
kind = "race"
on = ["claude:wf-hold-one", "claude:wf-hold-two"]
prompt = "Write your file."
verify = ["true"]

[[step]]
id = "after"
kind = "agent"
on = "claude"
prompt = "AFTERMARK"
"#,
    );
    let mut watcher = bed.watcher();
    let wf = bed.run("held", &[]).expect("the run opens");
    hold.await_parked(2);
    let (code, out) = bed.cli(&["cancel", &wf.0]);
    hold.release();
    assert_eq!(code, Some(0), "{out}");
    let result = bed.await_close(&mut watcher, &wf);
    assert_eq!(
        result.outcome,
        RunOutcome::Cancelled,
        "{}",
        result.scoreboard()
    );
    assert_eq!(result.steps[0].verdict, Some(StepVerdict::Cancelled));
    assert!(
        bed.requests_with("AFTERMARK").is_empty(),
        "nothing ran after the cancel"
    );

    let journal = records(&bed.project.journal());
    assert!(
        journal
            .iter()
            .any(|k| matches!(k, RecordKind::WorkflowCancelRequested(c) if c.wf_id == wf))
    );
    let by_workflow = journal
        .iter()
        .filter(|k| {
            matches!(k, RecordKind::CancelRequested(c)
            if c.by == marion_core::journal::CancelBy::Workflow { wf_id: wf.clone() })
        })
        .count();
    assert_eq!(
        by_workflow, 2,
        "each seat was cancelled as the workflow's cancel"
    );

    let (code, out) = bed.cli(&["cancel", &wf.0]);
    assert_ne!(code, Some(0), "a closed run is not cancelled again: {out}");
}

/// **(i) Each step node carries its share of the run's budget and clock**: a step's `share` of the
/// total, a parallel step's allowance split across its nodes, a step's own cap, and every step's
/// timeout clamped under the run's wall.
#[test]
fn step_nodes_carry_their_share_of_the_runs_budget_and_wall() {
    let script = Script {
        nodes: vec![
            node("SHAREMARK", &[], "a"),
            node("SPLITMARK", &[], "b"),
            node("CAPMARK", &[], "c"),
        ],
        ..Script::default()
    };
    let Some(bed) = bed("wf-budget", script) else {
        return;
    };
    bed.user_workflow(
        "budgeted",
        r#"schema = 1
name = "budgeted"
budget = { tokens = 1000, wall = "10m" }

[[step]]
id = "a"
kind = "agent"
on = "claude"
prompt = "SHAREMARK"
share = 0.3
timeout = "1h"

[[step]]
id = "b"
kind = "parallel"
on = ["claude", "claude"]
prompt = "SPLITMARK"

[[step]]
id = "c"
kind = "agent"
on = "claude"
prompt = "CAPMARK"
tokens = 100
"#,
    );
    let result = bed.run_to_close("budgeted", &[]);
    assert_eq!(
        result.outcome,
        RunOutcome::Succeeded,
        "{}",
        result.scoreboard()
    );
    let journal = records(&bed.project.journal());
    let mut caps: Vec<(u8, Option<u64>, Option<u64>)> = journal
        .iter()
        .filter_map(|k| match k {
            RecordKind::SpawnIntent(i) => i
                .workflow
                .as_ref()
                .map(|s| (s.step, i.budget.and_then(|b| b.tree_tokens), i.timeout_secs)),
            _ => None,
        })
        .collect();
    caps.sort();
    let tokens: Vec<_> = caps.iter().map(|(s, t, _)| (*s, *t)).collect();
    // The canned claude spends nothing, so each allowance is the whole of what is left.
    assert_eq!(
        tokens,
        [
            (0, Some(300)),
            (1, Some(500)),
            (1, Some(500)),
            (2, Some(100))
        ]
    );
    assert!(
        caps.iter().all(|(_, _, t)| t.is_some_and(|t| t <= 600)),
        "every timeout is under the ten-minute wall: {caps:?}"
    );
}
