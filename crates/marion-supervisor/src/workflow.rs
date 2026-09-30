//! **The supervisor's half of a workflow run**: the spec a run steps, the runs it is driving, and
//! what a step's nodes left — read off the journal's fold and their contract files, never kept in
//! a controller. The stepping itself is [`marion_core::workflow::next`]; the launching is the
//! handler's ([`crate::handler`]'s `drive_workflow`), on whichever thread a step node's end or a
//! restart happens on. There is no thread or timer per run.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use marion_core::contract::{AgentId, ExitStatus, TaskContract};
use marion_core::paths::ProjectDir;
use marion_core::registry::{ReplayedNode, ReplayedWorkflow};
use marion_core::workflow::{
    Field, StepKind, StepRow, StepState, StepVerdict, Values, WfState, Workflow, WorkflowId,
    WorkflowResult,
};
use serde::{Deserialize, Serialize};

/// What a run steps: the checked workflow, the operator's inputs, the tree it runs in, and the
/// file it was read from — snapshotted when it opened, so a restart never steps an edited file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spec {
    pub workflow: Workflow,
    pub inputs: BTreeMap<String, String>,
    pub repo: PathBuf,
    pub path: PathBuf,
    pub digest: String,
}

/// Write a run's spec, private to the operator: the inputs are the operator's words.
pub fn write_spec(project: &ProjectDir, id: &WorkflowId, spec: &Spec) -> std::io::Result<()> {
    let dir = project.workflow(id);
    crate::private_fs::create_dir_all(dir.path())?;
    let mut bytes = serde_json::to_vec_pretty(spec).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    crate::private_fs::write_atomic(&dir.spec(), &bytes)
}

pub fn read_spec(project: &ProjectDir, id: &WorkflowId) -> Option<Spec> {
    serde_json::from_slice(&std::fs::read(project.workflow(id).spec()).ok()?).ok()
}

pub fn write_result(project: &ProjectDir, result: &WorkflowResult) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(result).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    crate::private_fs::write_atomic(&project.workflow(&result.wf_id).result(), &bytes)
}

/// A closed run's scoreboard, as written; `None` while it is open or unreadable.
pub fn read_result(project: &ProjectDir, id: &WorkflowId) -> Option<WorkflowResult> {
    serde_json::from_slice(&std::fs::read(project.workflow(id).result()).ok()?).ok()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One open run: its spec, and whether a drive is stepping it.
#[derive(Debug)]
struct Open {
    spec: Arc<Spec>,
    driving: bool,
    /// A drive was asked for while one was running: the running one steps again before it lets go.
    again: bool,
}

/// Every run this supervisor is driving.
#[derive(Debug, Default)]
pub struct Workflows {
    open: Mutex<HashMap<WorkflowId, Open>>,
}

impl Workflows {
    pub fn open(&self, id: &WorkflowId, spec: Spec) {
        lock(&self.open).insert(
            id.clone(),
            Open {
                spec: Arc::new(spec),
                driving: false,
                again: false,
            },
        );
    }

    /// Claim the right to step `id` now, and its spec; `None` where it is not open here or is
    /// being stepped — which then steps again before it lets go, so no change is missed.
    pub fn begin_drive(&self, id: &WorkflowId) -> Option<Arc<Spec>> {
        let mut open = lock(&self.open);
        let o = open.get_mut(id)?;
        if o.driving {
            o.again = true;
            return None;
        }
        o.driving = true;
        Some(Arc::clone(&o.spec))
    }

    /// Release the claim; `true` where a drive was asked for meanwhile, and the caller steps again.
    pub fn end_drive(&self, id: &WorkflowId) -> bool {
        let mut open = lock(&self.open);
        let Some(o) = open.get_mut(id) else {
            return false;
        };
        o.driving = false;
        std::mem::take(&mut o.again)
    }

    pub fn close(&self, id: &WorkflowId) {
        lock(&self.open).remove(id);
    }

    pub fn is_open(&self, id: &WorkflowId) -> bool {
        lock(&self.open).contains_key(id)
    }
}

/// A step node's contract, where it wrote one.
fn contract_of(project: &ProjectDir, node: &ReplayedNode) -> Option<TaskContract> {
    let task = node.intent.as_ref()?.task_id.as_ref()?;
    serde_json::from_slice(&std::fs::read(project.agent(&node.agent_id).contract(task)).ok()?).ok()
}

/// Whether a step node is over: it ended, marion lost it, or its launch was aborted before a
/// process existed.
fn ended(node: &ReplayedNode) -> bool {
    node.state.is_exited()
        || node.reap_state == marion_core::node::ReapState::Orphaned
        || (node.spawn_aborted.is_some() && !node.spawn_confirmed)
}

/// **A step as the journal and its nodes' contracts have it**: decided, running, or not yet started
/// — and, for one whose nodes have all ended but that no decision names yet, the verdict they
/// earned, for the driver to journal.
pub enum Observed {
    State(StepState),
    Earned(StepVerdict),
}

pub fn observe(
    project: &ProjectDir,
    wf: &Workflow,
    run: &ReplayedWorkflow,
    nodes: &[ReplayedNode],
    step: usize,
) -> Observed {
    let Ok(s) = u8::try_from(step) else {
        return Observed::State(StepState::Pending);
    };
    if let Some(d) = run.decision(s) {
        return Observed::State(StepState::Decided(d.verdict));
    }
    let launched: Vec<&ReplayedNode> = run
        .step_nodes(s, 0)
        .into_iter()
        .filter_map(|id| nodes.iter().find(|n| &n.agent_id == id))
        .collect();
    if launched.is_empty() {
        return Observed::State(StepState::Pending);
    }
    if launched.len() < expected_nodes(&wf.steps[step].kind) || !launched.iter().all(|n| ended(n)) {
        return Observed::State(StepState::Running { round: 0 });
    }
    let all_ok = launched.iter().all(|n| {
        contract_of(project, n)
            .and_then(|c| c.completion)
            .is_some_and(|c| c.status == ExitStatus::Ok)
    });
    Observed::Earned(if all_ok {
        StepVerdict::Succeeded
    } else {
        StepVerdict::Failed
    })
}

/// How many nodes a step starts.
pub fn expected_nodes(kind: &StepKind) -> usize {
    match kind {
        StepKind::Parallel { on, .. } => on.len(),
        _ => 1,
    }
}

/// The run's state for [`marion_core::workflow::next`], with every step whose nodes have ended but
/// that is not yet decided counted as decided on what they earned — the caller journals those.
pub fn state(
    project: &ProjectDir,
    wf: &Workflow,
    run: &ReplayedWorkflow,
    nodes: &[ReplayedNode],
) -> (WfState, Vec<(usize, StepVerdict)>) {
    let mut state = WfState::new(wf);
    let mut earned = Vec::new();
    for i in 0..wf.steps.len() {
        state.steps[i] = match observe(project, wf, run, nodes, i) {
            Observed::State(s) => s,
            Observed::Earned(v) => {
                earned.push((i, v));
                StepState::Decided(v)
            }
        };
    }
    (state, earned)
}

/// **What a later step's prompt reads**: the run's inputs, and each earlier step's fields off its
/// nodes' contracts.
pub struct RunValues<'a> {
    pub spec: &'a Spec,
    pub project: &'a ProjectDir,
    pub run: &'a ReplayedWorkflow,
    pub nodes: &'a [ReplayedNode],
}

impl RunValues<'_> {
    fn contracts(&self, step: usize) -> Vec<(AgentId, TaskContract)> {
        let Ok(s) = u8::try_from(step) else {
            return Vec::new();
        };
        self.run
            .step_nodes(s, 0)
            .into_iter()
            .filter_map(|id| self.nodes.iter().find(|n| &n.agent_id == id))
            .filter_map(|n| contract_of(self.project, n).map(|c| (n.agent_id.clone(), c)))
            .collect()
    }
}

impl Values for RunValues<'_> {
    fn input(&self, name: &str) -> Option<String> {
        self.spec.inputs.get(name).cloned()
    }

    fn field(&self, step: usize, field: Field) -> Option<String> {
        let contracts = self.contracts(step);
        let many = contracts.len() > 1;
        let parts: Vec<String> = contracts
            .iter()
            .filter_map(|(id, c)| {
                let comp = c.completion.as_ref()?;
                let text = match field {
                    Field::Report => comp.narrative.as_ref().map(|n| n.value.clone()),
                    Field::Branch => comp.branch.clone(),
                    Field::Diffstat => (!comp.changed_paths.is_empty()).then(|| {
                        comp.changed_paths
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join("\n")
                    }),
                    Field::Findings => None,
                }?;
                Some(if many {
                    format!(
                        "{} ({}):\n{text}",
                        c.child.harness,
                        crate::tree::short_id(&id.0)
                    )
                } else {
                    text
                })
            })
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    fn step_id(&self, step: usize) -> Option<String> {
        self.spec.workflow.steps.get(step).map(|s| s.id.clone())
    }
}

/// The scoreboard of a run that closed with `outcome`.
pub fn result(
    project: &ProjectDir,
    spec: &Spec,
    id: &WorkflowId,
    run: &ReplayedWorkflow,
    nodes: &[ReplayedNode],
    outcome: marion_core::workflow::Outcome,
) -> WorkflowResult {
    let values = RunValues {
        spec,
        project,
        run,
        nodes,
    };
    let steps = spec
        .workflow
        .steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let s = u8::try_from(i).unwrap_or(u8::MAX);
            StepRow {
                id: step.id.clone(),
                kind: step.kind.word().into(),
                verdict: run.decision(s).map(|d| d.verdict),
                nodes: run.step_nodes(s, 0).into_iter().cloned().collect(),
                branch: step
                    .kind
                    .makes_a_branch()
                    .then(|| values.field(i, Field::Branch))
                    .flatten(),
            }
        })
        .collect();
    WorkflowResult {
        wf_id: id.clone(),
        name: spec.workflow.name.clone(),
        outcome,
        steps,
    }
}
