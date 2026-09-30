//! **Workflows, end to end: real claude nodes against marion's canned provider.**
//!
//! Each test writes a workflow into the bed's own user configuration (or, for the trust test, into
//! the repository), starts it over the socket with `workflow/run`, and waits for the run to close
//! by the badge its step nodes carry. What is asserted is what the run did: which steps ran and how
//! each ended, what a later step's prompt was given of an earlier step's output, and that nothing is
//! journaled for a workflow marion refuses.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use marion_core::journal::RecordKind;
use marion_core::paths::ProjectDir;
use marion_core::proto::notify::Event as Note;
use marion_core::proto::params::WorkflowRunParams;
use marion_core::proto::{Call, Method, MethodResult};
use marion_core::workflow::{Outcome as RunOutcome, StepVerdict, WorkflowId};
use marion_provider::{CannedServer, Config, NodeScript, Script, ScriptedCall};
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
        .map(|f| ScriptedCall::new("Write", json!({"file_path": f, "content": "work\n"})))
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
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: reqlog.clone(),
        script,
    })
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
        let paths = paths_for(&self.state, &self.repo);
        let mut watcher = Client::dial(&paths);
        watcher.read_bound(BOUND);
        watcher.tree();
        let wf = self.run(name, inputs).expect("the run opens");
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
