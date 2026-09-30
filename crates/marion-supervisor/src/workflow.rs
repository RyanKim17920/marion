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
use marion_core::registry::{ReplayedNode, ReplayedRace, ReplayedWorkflow};
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
/// — or, where it is not decided yet, the verdict its nodes earned (for the driver to journal), or
/// the next node a review step's round needs (for the driver to launch).
pub enum Observed {
    State(StepState),
    Earned(StepVerdict),
    Act(Act),
}

/// A review step's next node, within the step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    /// Review `target`'s work in `round` (0-based).
    Review { round: u8, target: AgentId },
    /// Fix what `reviewer` found in `target`'s work, in `round`.
    Fix {
        round: u8,
        target: AgentId,
        reviewer: AgentId,
    },
}

/// A review step's reviewer in a round is its part 0; the fixer after it is part 1.
pub const REVIEWER: u8 = 0;
pub const FIXER: u8 = 1;

/// The race a race step's seats belong to.
fn race_of<'a>(seats: &[&ReplayedNode], races: &'a [ReplayedRace]) -> Option<&'a ReplayedRace> {
    let id = seats
        .iter()
        .find_map(|n| n.intent.as_ref()?.race.as_ref())
        .map(|r| &r.race_id)?;
    races.iter().find(|r| &r.race_id == id)
}

/// How many nodes a step starts at once.
pub fn expected_nodes(kind: &StepKind) -> usize {
    match kind {
        StepKind::Parallel { on, .. } => on.len(),
        _ => 1,
    }
}

/// Whether a step node ended with its contract `Ok`.
fn succeeded(project: &ProjectDir, node: &ReplayedNode) -> bool {
    contract_of(project, node)
        .and_then(|c| c.completion)
        .is_some_and(|c| c.status == ExitStatus::Ok)
}

/// A run's state, the verdicts its steps earned undecided, and the review nodes it needs next:
/// what [`RunValues::state`] answers.
pub type Stepped = (WfState, Vec<(usize, StepVerdict)>, Vec<(usize, Act)>);

/// **What one run is, read off the journal's fold and its step nodes' contracts** — the state its
/// stepping needs, and the values a later step's prompt reads.
pub struct RunValues<'a> {
    pub spec: &'a Spec,
    pub project: &'a ProjectDir,
    pub run: &'a ReplayedWorkflow,
    pub nodes: &'a [ReplayedNode],
    pub races: &'a [ReplayedRace],
}

impl RunValues<'_> {
    fn node(&self, id: &AgentId) -> Option<&ReplayedNode> {
        self.nodes.iter().find(|n| &n.agent_id == id)
    }

    /// Every node `step` launched, in intent order.
    pub fn step_nodes(&self, step: usize) -> Vec<&ReplayedNode> {
        self.run
            .nodes
            .iter()
            .filter(|(seat, _)| usize::from(seat.step) == step)
            .filter_map(|(_, id)| self.node(id))
            .collect()
    }

    /// The node in `step`'s seat `(round, part)`.
    fn seat(&self, step: usize, round: u8, part: u8) -> Option<&ReplayedNode> {
        self.run
            .nodes
            .iter()
            .find(|(s, _)| usize::from(s.step) == step && s.round == round && s.part == part)
            .and_then(|(_, id)| self.node(id))
    }

    /// **The node whose work `step` stands for**: its one node, a race's winner, or a review's
    /// latest fix that passed (else the work it reviewed).
    pub fn work_node(&self, step: usize) -> Option<&ReplayedNode> {
        match &self.spec.workflow.steps.get(step)?.kind {
            StepKind::Agent { .. } => self.step_nodes(step).into_iter().next(),
            StepKind::Race { .. } => {
                let seats = self.step_nodes(step);
                let winner = race_of(&seats, self.races)?
                    .decided
                    .as_ref()?
                    .winner
                    .clone()?;
                self.node(&winner)
            }
            StepKind::Review { of, max_rounds, .. } => {
                let mut work = self.work_node(*of)?;
                for r in 0..*max_rounds {
                    match self.seat(step, r, FIXER) {
                        Some(fix) if ended(fix) && succeeded(self.project, fix) => work = fix,
                        _ => break,
                    }
                }
                Some(work)
            }
            // What a land step landed is the work it names.
            StepKind::Land { of, .. } => self.work_node(*of),
            StepKind::Parallel { .. } => None,
        }
    }

    /// **What a land step of `of` lands**: the branch and commit that step's work left, or `None`
    /// where it left none.
    pub fn landing(&self, of: usize) -> Option<(String, marion_core::contract::Oid)> {
        let completion = contract_of(self.project, self.work_node(of)?)?.completion?;
        Some((completion.branch?, completion.commit?))
    }

    /// The contracts a step's fields are read from: each of a parallel step's nodes', or the one
    /// node its work is ([`Self::work_node`]).
    fn contracts(&self, step: usize) -> Vec<(AgentId, TaskContract)> {
        let chosen: Vec<&ReplayedNode> = match self.spec.workflow.steps.get(step).map(|d| &d.kind) {
            Some(StepKind::Parallel { .. }) => self.step_nodes(step),
            Some(_) => self.work_node(step).into_iter().collect(),
            None => Vec::new(),
        };
        chosen
            .into_iter()
            .filter_map(|n| contract_of(self.project, n).map(|c| (n.agent_id.clone(), c)))
            .collect()
    }

    /// The last findings a review step's reviewers reported, as a list.
    fn findings(&self, step: usize) -> Option<String> {
        let StepKind::Review { max_rounds, .. } = self.spec.workflow.steps.get(step)?.kind else {
            return None;
        };
        let last = (0..max_rounds)
            .rev()
            .find_map(|r| self.seat(step, r, REVIEWER))?;
        findings_text(&contract_of(self.project, last)?)
    }

    /// **A step as the journal and its nodes have it.**
    pub fn observe(&self, step: usize) -> Observed {
        let Ok(s) = u8::try_from(step) else {
            return Observed::State(StepState::Pending);
        };
        if let Some(d) = self.run.decision(s) {
            return Observed::State(StepState::Decided(d.verdict));
        }
        let running = Observed::State(StepState::Running { round: 0 });
        let kind = &self.spec.workflow.steps[step].kind;
        if let StepKind::Review { of, max_rounds, .. } = kind {
            return self.observe_review(step, *of, *max_rounds);
        }
        let launched = self.step_nodes(step);
        if launched.is_empty() {
            return Observed::State(StepState::Pending);
        }
        // A race step is decided by its race, which may stop a seat or wait for every one.
        if matches!(kind, StepKind::Race { .. }) {
            return match race_of(&launched, self.races).and_then(|r| r.decided.as_ref()) {
                Some(d) => Observed::Earned(if d.winner.is_some() {
                    StepVerdict::Succeeded
                } else {
                    StepVerdict::Failed
                }),
                None => running,
            };
        }
        if launched.len() < expected_nodes(kind) || !launched.iter().all(|n| ended(n)) {
            return running;
        }
        Observed::Earned(if launched.iter().all(|n| succeeded(self.project, n)) {
            StepVerdict::Succeeded
        } else {
            StepVerdict::Failed
        })
    }

    /// A review step, round by round: each reviewer's decision (off its contract's tally), the
    /// fixer after a blocking one, and the review of that fix — until one is clean, a fix fails, or
    /// the last round still blocks.
    fn observe_review(&self, step: usize, of: usize, max_rounds: u8) -> Observed {
        let running = Observed::State(StepState::Running { round: 0 });
        // Not started: nothing is known of the work it will review yet.
        if self.step_nodes(step).is_empty() {
            return Observed::State(StepState::Pending);
        }
        let Some(mut target) = self.work_node(of) else {
            return Observed::Earned(StepVerdict::Failed);
        };
        for r in 0..max_rounds {
            let Some(rev) = self.seat(step, r, REVIEWER) else {
                return if r == 0 {
                    Observed::State(StepState::Pending)
                } else {
                    Observed::Act(Act::Review {
                        round: r,
                        target: target.agent_id.clone(),
                    })
                };
            };
            if !ended(rev) {
                return running;
            }
            let decision = rev
                .contracts
                .last()
                .and_then(|c| c.review)
                .map(|t| t.decision);
            match decision {
                Some(marion_core::review::Decision::Allow) => {
                    return Observed::Earned(StepVerdict::Clean);
                }
                Some(marion_core::review::Decision::Block) if r + 1 == max_rounds => {
                    return Observed::Earned(StepVerdict::Blocked);
                }
                Some(marion_core::review::Decision::Block) => match self.seat(step, r, FIXER) {
                    None => {
                        return Observed::Act(Act::Fix {
                            round: r,
                            target: target.agent_id.clone(),
                            reviewer: rev.agent_id.clone(),
                        });
                    }
                    Some(fix) if !ended(fix) => return running,
                    Some(fix) if succeeded(self.project, fix) => target = fix,
                    Some(_) => return Observed::Earned(StepVerdict::Failed),
                },
                // A reviewer whose report marion could not read decides nothing.
                None => return Observed::Earned(StepVerdict::Failed),
            }
        }
        Observed::Earned(StepVerdict::Blocked)
    }

    /// The run's state for [`marion_core::workflow::next`]: every step as it stands, the steps
    /// whose nodes have earned a verdict no decision records yet (for the driver to journal), and
    /// the review nodes a running review step needs next (for the driver to launch). In a run the
    /// operator cancelled, a step that ended without passing was cancelled, and a review launches
    /// no further round.
    pub fn state(&self) -> Stepped {
        let wf = &self.spec.workflow;
        let cancelled = self.run.cancel_requested;
        let mut state = WfState::new(wf);
        state.cancelled = cancelled;
        let (mut earned, mut acts) = (Vec::new(), Vec::new());
        for i in 0..wf.steps.len() {
            state.steps[i] = match self.observe(i) {
                Observed::State(s) => s,
                Observed::Earned(v) => {
                    let v = match v {
                        StepVerdict::Succeeded | StepVerdict::Clean => v,
                        _ if cancelled => StepVerdict::Cancelled,
                        _ => v,
                    };
                    earned.push((i, v));
                    StepState::Decided(v)
                }
                Observed::Act(_) if cancelled => {
                    earned.push((i, StepVerdict::Cancelled));
                    StepState::Decided(StepVerdict::Cancelled)
                }
                Observed::Act(a) => {
                    acts.push((i, a));
                    StepState::Running { round: 0 }
                }
            };
        }
        (state, earned, acts)
    }

    /// **What the run has committed of its token budget**, or `step`'s part of it: what each node
    /// that ended spent, and for each node still running the whole allowance it was given (or its
    /// spend so far, if more, or with none).
    pub fn committed(&self, step: Option<usize>) -> u64 {
        self.run
            .nodes
            .iter()
            .filter(|(seat, _)| step.is_none_or(|s| usize::from(seat.step) == s))
            .filter_map(|(_, id)| self.node(id))
            .map(|n| {
                let spent = n.usage.map_or(0, |u| u.total());
                let held = n.intent.as_ref().and_then(|i| i.budget?.tree_tokens);
                match held {
                    Some(held) if !ended(n) => held.max(spent),
                    _ => spent,
                }
            })
            .fold(0, u64::saturating_add)
    }

    /// **The commit a step builds on**: the work of the latest earlier step that left a branch and
    /// passed, or `None` for the repository's `HEAD`.
    pub fn base_for(&self, step: usize) -> Option<marion_core::contract::Oid> {
        (0..step).rev().find_map(|i| {
            let def = self.spec.workflow.steps.get(i)?;
            let passed = matches!(
                self.run.decision(u8::try_from(i).ok()?).map(|d| d.verdict),
                Some(StepVerdict::Succeeded | StepVerdict::Clean)
            );
            if !def.kind.makes_a_branch() || !passed {
                return None;
            }
            let work = self.work_node(i)?;
            contract_of(self.project, work)?.completion?.commit
        })
    }
}

/// A reviewer's grounded findings as a list a fixer or a later prompt reads, blocking ones first.
pub fn findings_text(reviewer: &TaskContract) -> Option<String> {
    let findings = reviewer.completion.as_ref()?.findings.as_ref()?;
    let mut lines: Vec<String> = findings
        .findings
        .iter()
        .map(|f| {
            format!(
                "- [{:?}{}] {}{}: {}{}",
                f.severity,
                if f.grounded {
                    ""
                } else {
                    ", not in the change"
                },
                f.file,
                f.line.map(|l| format!(":{l}")).unwrap_or_default(),
                f.claim,
                if f.recommendation.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", f.recommendation)
                }
            )
        })
        .collect();
    if !findings.summary.is_empty() {
        lines.insert(0, findings.summary.clone());
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

impl Values for RunValues<'_> {
    fn input(&self, name: &str) -> Option<String> {
        self.spec.inputs.get(name).cloned()
    }

    fn field(&self, step: usize, field: Field) -> Option<String> {
        if field == Field::Findings {
            return self.findings(step);
        }
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
                    Field::Findings => return None,
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
    races: &[ReplayedRace],
    outcome: marion_core::workflow::Outcome,
) -> WorkflowResult {
    let values = RunValues {
        spec,
        project,
        run,
        nodes,
        races,
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
                nodes: values
                    .step_nodes(i)
                    .into_iter()
                    .map(|n| n.agent_id.clone())
                    .collect(),
                branch: step
                    .kind
                    .makes_a_branch()
                    .then(|| values.field(i, Field::Branch))
                    .flatten(),
                note: run.decision(s).and_then(|d| d.note.clone()),
                work: values.work_node(i).and_then(|n| {
                    let intent = n.intent.as_ref()?;
                    Some(marion_core::workflow::StepWork {
                        agent_id: n.agent_id.clone(),
                        task_id: intent.task_id.clone()?,
                        agent_type: intent.agent_type.clone(),
                    })
                }),
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
