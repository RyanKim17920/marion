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
                        crate::node_label::short_id(&id.0)
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use marion_core::contract::{
        ExitStatus, Glob, Oid, ProcessExit, RepoIdentity, TaskId, TokenUsage, Workspace,
    };
    use marion_core::encoding::SystemTime;
    use marion_core::harness::Harness;
    use marion_core::journal::{
        ContractPersisted, Exited, JournalRecord, RaceDecided, RecordKind, SpawnIntent,
        UsageRecorded, WorkflowCancelRequested, WorkflowOpened, WorkflowStepDecided, WriterId,
        encode,
    };
    use marion_core::review::{Decision, Finding, Findings, ModelVerdict, ReviewTally, Severity};
    use marion_core::workflow::WorkflowSeat;

    fn id(s: &str) -> AgentId {
        AgentId(s.into())
    }

    /// **One workflow run, as a journal and a project dir hold it**: the records a run's nodes
    /// leave, replayed for real, and the contracts they write, on disk where [`contract_of`] reads.
    pub(crate) struct Fx {
        pub dir: marion_testsupport::Scratch,
        pub project: ProjectDir,
        pub spec: Spec,
        pub wf: WorkflowId,
        pub opened: WorkflowOpened,
        pub races: Vec<ReplayedRace>,
        records: Vec<RecordKind>,
    }

    impl Fx {
        pub fn new(tag: &str, toml: &str) -> Fx {
            let dir = marion_testsupport::scratch(tag);
            let project = ProjectDir::from_hash(&dir, "proj");
            let workflow = marion_core::workflow::parse(toml).expect("the fixture parses");
            let wf = WorkflowId("wf-1".into());
            let opened = WorkflowOpened {
                wf_id: wf.clone(),
                name: workflow.name.clone(),
                digest: "ab".into(),
                requester: id(marion_core::workflow::OPERATOR),
                base: Some("base0".into()),
                deadline: None,
                budget_tokens: workflow.budget.tokens,
                step_ids: workflow.steps.iter().map(|s| s.id.clone()).collect(),
            };
            let spec = Spec {
                workflow,
                inputs: BTreeMap::new(),
                repo: dir.join("repo"),
                path: dir.join("w.toml"),
                digest: "ab".into(),
            };
            Fx {
                dir,
                project,
                spec,
                wf,
                opened,
                races: Vec::new(),
                records: Vec::new(),
            }
        }

        /// A node in `step`'s seat `(round, part)`, its task `t-<agent>`, holding `tree_tokens`.
        pub fn seat(
            &mut self,
            agent: &str,
            step: u8,
            round: u8,
            part: u8,
            tree_tokens: Option<u64>,
        ) {
            self.records.push(RecordKind::SpawnIntent(SpawnIntent {
                agent_id: id(agent),
                parent_id: None,
                agent_type: "codex".into(),
                harness: Harness::Codex,
                depth: 1,
                task_id: Some(TaskId(format!("t-{agent}"))),
                timeout_secs: None,
                verification: vec![],
                review_of: None,
                race: None,
                budget: tree_tokens.map(|t| marion_core::budget::Budget {
                    tree_tokens: Some(t),
                    ..Default::default()
                }),
                workflow: Some(WorkflowSeat {
                    wf_id: self.wf.clone(),
                    step,
                    round,
                    part,
                }),
                trust: Default::default(),
            }));
        }

        pub fn node(&mut self, agent: &str, step: u8) {
            self.seat(agent, step, 0, 0, None);
        }

        pub fn exit(&mut self, agent: &str) {
            self.records.push(RecordKind::Exited(Exited {
                agent_id: id(agent),
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: String::new(),
                },
            }));
        }

        pub fn usage(&mut self, agent: &str, output: u64) {
            self.records.push(RecordKind::UsageRecorded(UsageRecorded {
                agent_id: id(agent),
                usage: TokenUsage {
                    input: 0,
                    output,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: None,
                },
                turns: vec![],
            }));
        }

        /// A reviewer's contract on record with `decision` as its tally.
        pub fn review(&mut self, agent: &str, decision: Decision) {
            self.records
                .push(RecordKind::ContractPersisted(ContractPersisted {
                    agent_id: id(agent),
                    task_id: TaskId(format!("t-{agent}")),
                    requester: id(marion_core::workflow::OPERATOR),
                    status: Some(ExitStatus::Ok),
                    review: Some(ReviewTally {
                        findings: 1,
                        blocking: usize::from(decision == Decision::Block),
                        decision,
                    }),
                    changed: None,
                }));
        }

        pub fn decide(&mut self, step: u8, verdict: StepVerdict) {
            self.records
                .push(RecordKind::WorkflowStepDecided(WorkflowStepDecided {
                    wf_id: self.wf.clone(),
                    step,
                    round: 0,
                    verdict,
                    nodes: vec![],
                    note: None,
                }));
        }

        pub fn cancel(&mut self) {
            self.records.push(RecordKind::WorkflowCancelRequested(
                WorkflowCancelRequested {
                    wf_id: self.wf.clone(),
                },
            ));
        }

        /// The node's contract file, ended `status`, its work on `branch` at `commit`.
        pub fn contract(
            &self,
            agent: &str,
            status: ExitStatus,
            landed: Option<(&str, &str)>,
            findings: Option<Findings>,
        ) {
            let task = TaskId(format!("t-{agent}"));
            let mut c = crate::spawn::build_contract(
                task.clone(),
                id(marion_core::workflow::OPERATOR),
                Harness::Codex,
                RepoIdentity {
                    git_common_dir: None,
                    head_branch: None,
                },
                None::<Oid>,
                Workspace::Worktree {
                    path: self.dir.join("wt"),
                    branch: format!("marion/t-{agent}"),
                },
                &format!("the task {agent} was given"),
                &[],
                &[Glob("**".into())],
                &[Glob("**".into())],
                marion_core::encoding::Duration(std::time::Duration::from_secs(60)),
                SystemTime::from_unix_millis(1_000),
                &crate::spawn::ChildOutcome {
                    exit_code: Some(0),
                    ..crate::spawn::ChildOutcome::default()
                },
                None,
                None,
                vec![],
                vec![],
            );
            let done = c.completion.as_mut().expect("an ended contract");
            done.status = status;
            done.branch = landed.map(|(b, _)| b.into());
            done.commit = landed.map(|(_, o)| Oid(o.into()));
            done.findings = findings;
            let path = self.project.agent(&id(agent)).contract(&task);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, serde_json::to_vec(&c).unwrap()).unwrap();
        }

        /// `agent`'s contract ended `Ok` with nothing landed, and the node ended.
        pub fn passed(&mut self, agent: &str) {
            self.contract(agent, ExitStatus::Ok, None, None);
            self.exit(agent);
        }

        /// The journal as written, replayed the way the supervisor's fold reads it.
        pub fn replay(&self) -> marion_core::registry::Replay {
            let kinds = std::iter::once(RecordKind::WorkflowOpened(self.opened.clone()))
                .chain(self.records.iter().cloned());
            let bytes: Vec<u8> = kinds
                .enumerate()
                .flat_map(|(seq, kind)| {
                    encode(&JournalRecord {
                        writer: WriterId("w".into()),
                        seq: seq as u64,
                        ts: SystemTime::from_unix_millis(1_000 + seq as u64),
                        mono_ns: seq as u64,
                        provenance: marion_core::ir::Provenance::marion(),
                        src_seq: None,
                        kind,
                    })
                    .unwrap()
                })
                .collect();
            marion_core::registry::replay(&bytes)
        }

        /// `f` over the run as it stands.
        pub fn read<R>(&self, f: impl FnOnce(&RunValues<'_>) -> R) -> R {
            let r = self.replay();
            let run = r.workflow(&self.wf).expect("the run is on record").clone();
            f(&RunValues {
                spec: &self.spec,
                project: &self.project,
                run: &run,
                nodes: r.nodes(),
                races: &self.races,
            })
        }

        fn observed(&self, step: usize) -> String {
            self.read(|v| describe(v.observe(step)))
        }
    }

    fn describe(o: Observed) -> String {
        match o {
            Observed::State(s) => format!("{s:?}"),
            Observed::Earned(v) => format!("earned {v:?}"),
            Observed::Act(a) => format!("{a:?}"),
        }
    }

    const PLAN_IMPL: &str = r#"
schema = 1
name = "w"

[[step]]
id = "plan"
kind = "agent"
on = "claude"
read_only = true
prompt = "plan"

[[step]]
id = "impl"
kind = "agent"
on = "codex"
prompt = "impl"
"#;

    fn with_review(rounds: u8) -> String {
        format!(
            "{PLAN_IMPL}\n[[step]]\nid = \"gate\"\nkind = \"review\"\nof = \"impl\"\nmax_rounds = {rounds}\n"
        )
    }

    /// **A drive asked for while one runs is never lost**: the second asker is turned away, the
    /// running drive is told to step again once, and a closed run is no one's to drive.
    #[test]
    fn a_drive_asked_for_meanwhile_steps_again_once() {
        let w = Workflows::default();
        let wf = WorkflowId("wf-1".into());
        assert!(w.begin_drive(&wf).is_none(), "a run not open here");
        assert!(!w.end_drive(&wf));
        let fx = Fx::new("wf-drive", PLAN_IMPL);
        w.open(&wf, fx.spec.clone());
        assert!(w.is_open(&wf));
        let spec = w.begin_drive(&wf).expect("the first drive claims it");
        assert_eq!(spec.workflow.name, "w");
        assert!(w.begin_drive(&wf).is_none(), "one drive at a time");
        assert!(w.begin_drive(&wf).is_none());
        assert!(w.end_drive(&wf), "asked for meanwhile: step again");
        assert!(w.begin_drive(&wf).is_some(), "released, it claims again");
        assert!(!w.end_drive(&wf), "nothing asked for this time");
        w.close(&wf);
        assert!(!w.is_open(&wf));
        assert!(w.begin_drive(&wf).is_none());
    }

    /// **An agent step is pending, running, then earns its contract's verdict**: no node is
    /// pending, a node still running is running, and an ended node's `Ok` contract passes it
    /// while a failed or missing contract fails it. A recorded decision is what it says.
    #[test]
    fn an_agent_step_earns_what_its_node_left() {
        let mut fx = Fx::new("wf-agent", PLAN_IMPL);
        assert_eq!(fx.observed(0), "Pending");
        fx.node("p", 0);
        assert_eq!(fx.observed(0), "Running { round: 0 }");
        fx.exit("p");
        assert_eq!(fx.observed(0), "earned Failed", "ended without a contract");
        fx.contract("p", ExitStatus::Failed, None, None);
        assert_eq!(fx.observed(0), "earned Failed");
        fx.contract("p", ExitStatus::Ok, None, None);
        assert_eq!(fx.observed(0), "earned Succeeded");
        fx.decide(0, StepVerdict::Skipped);
        assert_eq!(
            fx.observed(0),
            "Decided(Skipped)",
            "the journal's decision wins"
        );
    }

    /// **A parallel step waits for every node it starts**, and passes only if each passed.
    #[test]
    fn a_parallel_step_waits_for_all_its_nodes_and_needs_each_to_pass() {
        let toml = r#"
schema = 1
name = "w"

[[step]]
id = "ask"
kind = "parallel"
on = ["claude", "codex"]
prompt = "ask"
"#;
        let mut fx = Fx::new("wf-parallel", toml);
        fx.seat("a", 0, 0, 0, None);
        fx.passed("a");
        assert_eq!(
            fx.observed(0),
            "Running { round: 0 }",
            "one of two launched"
        );
        fx.seat("b", 0, 0, 1, None);
        assert_eq!(fx.observed(0), "Running { round: 0 }", "one still running");
        fx.contract("b", ExitStatus::Failed, None, None);
        fx.exit("b");
        assert_eq!(fx.observed(0), "earned Failed");
        fx.contract("b", ExitStatus::Ok, None, None);
        assert_eq!(fx.observed(0), "earned Succeeded");
        assert!(
            fx.read(|v| v.work_node(0).is_none()),
            "a parallel step stands for no one node's work"
        );
    }

    /// **A race step is its race's decision**: open is running, a winner passes it and is its
    /// work, and a race nobody won fails it.
    #[test]
    fn a_race_step_is_decided_by_its_race() {
        let toml = r#"
schema = 1
name = "w"

[[step]]
id = "impl"
kind = "race"
on = ["claude", "codex"]
prompt = "impl"
verify = ["true"]
"#;
        let mut fx = Fx::new("wf-race", toml);
        let race = marion_core::race::RaceId("r-1".into());
        for (part, agent) in ["s1", "s2"].into_iter().enumerate() {
            fx.seat(agent, 0, 0, u8::try_from(part).unwrap(), None);
            let marion_core::journal::RecordKind::SpawnIntent(i) = fx.records.last_mut().unwrap()
            else {
                unreachable!()
            };
            i.race = Some(marion_core::race::RaceSeat {
                race_id: race.clone(),
                role: marion_core::race::RaceRole::Candidate(u8::try_from(part + 1).unwrap()),
            });
        }
        fx.races = vec![ReplayedRace {
            race_id: race.clone(),
            opened: None,
            seats: vec![(1, id("s1")), (2, id("s2"))],
            decided: None,
        }];
        assert_eq!(fx.observed(0), "Running { round: 0 }");
        let decided = |winner: Option<&str>| RaceDecided {
            race_id: race.clone(),
            winner: winner.map(id),
            decided_by: marion_core::race::DecidedBy::Verification,
            verdicts: vec![],
        };
        fx.races[0].decided = Some(decided(Some("s2")));
        assert_eq!(fx.observed(0), "earned Succeeded");
        assert_eq!(
            fx.read(|v| v.work_node(0).map(|n| n.agent_id.0.clone())),
            Some("s2".into()),
            "the winner's work is the step's"
        );
        fx.races[0].decided = Some(decided(None));
        assert_eq!(fx.observed(0), "earned Failed");
        assert!(fx.read(|v| v.work_node(0).is_none()));
    }

    /// **A review, round by round**: the reviewer running is running; a block asks for the fix of
    /// the work it reviewed; a passing fix asks for the next round's review of the fix; a block on
    /// the last round is blocked. The step's work is the latest fix that passed.
    #[test]
    fn a_review_rounds_through_fix_and_re_review_until_its_last_round() {
        let mut fx = Fx::new("wf-review", &with_review(2));
        fx.node("impl", 1);
        fx.passed("impl");
        assert_eq!(fx.observed(2), "Pending", "nothing of the review launched");
        fx.seat("rev0", 2, 0, REVIEWER, None);
        assert_eq!(fx.observed(2), "Running { round: 0 }");
        fx.review("rev0", Decision::Block);
        fx.exit("rev0");
        assert_eq!(
            fx.observed(2),
            "Fix { round: 0, target: AgentId(\"impl\"), reviewer: AgentId(\"rev0\") }"
        );
        fx.seat("fix0", 2, 0, FIXER, None);
        assert_eq!(fx.observed(2), "Running { round: 0 }", "the fix is running");
        fx.contract("fix0", ExitStatus::Ok, Some(("marion/t-fix0", "f1x")), None);
        fx.exit("fix0");
        assert_eq!(
            fx.observed(2),
            "Review { round: 1, target: AgentId(\"fix0\") }",
            "the next round reviews the fix, not the work"
        );
        assert_eq!(
            fx.read(|v| v.work_node(2).map(|n| n.agent_id.0.clone())),
            Some("fix0".into())
        );
        fx.seat("rev1", 2, 1, REVIEWER, None);
        fx.review("rev1", Decision::Block);
        fx.exit("rev1");
        assert_eq!(
            fx.observed(2),
            "earned Blocked",
            "the last round still blocks"
        );
    }

    /// **A clean review ends it, a failed fix fails it, and a report marion cannot read decides
    /// nothing in the step's favour.** A review whose work left no node fails before it starts.
    #[test]
    fn a_review_is_clean_or_failed_by_what_its_round_left() {
        let mut fx = Fx::new("wf-review-clean", &with_review(3));
        fx.node("impl", 1);
        fx.passed("impl");
        fx.seat("rev0", 2, 0, REVIEWER, None);
        fx.review("rev0", Decision::Allow);
        fx.exit("rev0");
        assert_eq!(fx.observed(2), "earned Clean");

        let mut fx = Fx::new("wf-review-fixfail", &with_review(3));
        fx.node("impl", 1);
        fx.passed("impl");
        fx.seat("rev0", 2, 0, REVIEWER, None);
        fx.review("rev0", Decision::Block);
        fx.exit("rev0");
        fx.seat("fix0", 2, 0, FIXER, None);
        fx.contract("fix0", ExitStatus::Failed, None, None);
        fx.exit("fix0");
        assert_eq!(fx.observed(2), "earned Failed", "a failed fix");
        assert_eq!(
            fx.read(|v| v.work_node(2).map(|n| n.agent_id.0.clone())),
            Some("impl".into()),
            "a failed fix is not the step's work"
        );

        let mut fx = Fx::new("wf-review-unread", &with_review(3));
        fx.node("impl", 1);
        fx.passed("impl");
        fx.seat("rev0", 2, 0, REVIEWER, None);
        fx.exit("rev0");
        assert_eq!(fx.observed(2), "earned Failed", "no tally on record");

        let mut fx = Fx::new("wf-review-nowork", &with_review(3));
        fx.seat("rev0", 2, 0, REVIEWER, None);
        assert_eq!(fx.observed(2), "earned Failed", "nothing to review");
    }

    /// **A cancelled run cancels what did not pass, and starts no further round**: a step that
    /// failed is cancelled, one that passed keeps its verdict, and a review's next node is not
    /// asked for but the step decided cancelled.
    #[test]
    fn a_cancelled_run_cancels_what_did_not_pass_and_asks_for_no_round() {
        let mut fx = Fx::new("wf-cancel", &with_review(2));
        fx.node("p", 0);
        fx.passed("p");
        fx.node("impl", 1);
        fx.contract("impl", ExitStatus::Failed, None, None);
        fx.exit("impl");
        fx.cancel();
        let (state, earned, acts) = fx.read(|v| v.state());
        assert!(state.cancelled);
        assert_eq!(
            earned,
            vec![(0, StepVerdict::Succeeded), (1, StepVerdict::Cancelled)]
        );
        assert!(acts.is_empty());

        let mut fx = Fx::new("wf-cancel-review", &with_review(2));
        fx.node("impl", 1);
        fx.passed("impl");
        fx.decide(1, StepVerdict::Succeeded);
        fx.seat("rev0", 2, 0, REVIEWER, None);
        fx.review("rev0", Decision::Block);
        fx.exit("rev0");
        let (_, earned, acts) = fx.read(|v| v.state());
        assert_eq!(acts.len(), 1, "uncancelled, the fix is asked for: {acts:?}");
        assert!(earned.is_empty());
        fx.cancel();
        let (state, earned, acts) = fx.read(|v| v.state());
        assert!(acts.is_empty(), "{acts:?}");
        assert_eq!(earned, vec![(2, StepVerdict::Cancelled)]);
        assert_eq!(state.steps[2], StepState::Decided(StepVerdict::Cancelled));
    }

    /// **A land step lands its work's branch and commit**, and nothing where it left none.
    #[test]
    fn landing_reads_the_works_branch_and_commit() {
        let mut fx = Fx::new("wf-landing", PLAN_IMPL);
        fx.node("impl", 1);
        fx.passed("impl");
        assert_eq!(fx.read(|v| v.landing(1)), None, "nothing landed");
        fx.contract(
            "impl",
            ExitStatus::Ok,
            Some(("marion/t-impl", "c0mm1t")),
            None,
        );
        assert_eq!(
            fx.read(|v| v.landing(1)),
            Some(("marion/t-impl".to_string(), Oid("c0mm1t".into())))
        );
        assert_eq!(fx.read(|v| v.landing(0)), None, "a step that never ran");
    }

    /// **What the run has committed of its budget**: an ended node what it spent, a running node
    /// the whole allowance it holds (or its spend, where more), and a step's part only its own.
    #[test]
    fn committed_holds_a_running_nodes_whole_allowance() {
        let mut fx = Fx::new("wf-committed", PLAN_IMPL);
        fx.seat("p", 0, 0, 0, Some(1_000));
        fx.usage("p", 300);
        fx.exit("p");
        fx.seat("i", 1, 0, 0, Some(5_000));
        fx.usage("i", 200);
        fx.seat("free", 1, 0, 1, None);
        fx.usage("free", 7);
        assert_eq!(fx.read(|v| v.committed(Some(0))), 300, "ended: its spend");
        assert_eq!(
            fx.read(|v| v.committed(Some(1))),
            5_007,
            "running: its allowance; no allowance: its spend"
        );
        assert_eq!(fx.read(|v| v.committed(None)), 5_307);
        fx.usage("i", 6_000);
        assert_eq!(
            fx.read(|v| v.committed(Some(1))),
            6_207,
            "a spend past the allowance counts whole"
        );
    }

    /// **A step builds on the latest earlier step that passed and left a commit**: not a failed
    /// one, not a read-only one, and the repository's `HEAD` where there is none.
    #[test]
    fn base_for_is_the_latest_passing_branch_steps_commit() {
        let toml = format!(
            "{PLAN_IMPL}\n[[step]]\nid = \"redo\"\nkind = \"agent\"\non = \"codex\"\nprompt = \"r\"\n\n[[step]]\nid = \"last\"\nkind = \"agent\"\non = \"codex\"\nprompt = \"l\"\n"
        );
        let mut fx = Fx::new("wf-base", &toml);
        fx.node("p", 0);
        fx.contract("p", ExitStatus::Ok, Some(("marion/t-p", "p0")), None);
        fx.exit("p");
        fx.decide(0, StepVerdict::Succeeded);
        assert_eq!(
            fx.read(|v| v.base_for(1)),
            None,
            "a read-only step is no base"
        );
        fx.node("impl", 1);
        fx.contract("impl", ExitStatus::Ok, Some(("marion/t-impl", "i1")), None);
        fx.exit("impl");
        assert_eq!(fx.read(|v| v.base_for(2)), None, "not decided yet");
        fx.decide(1, StepVerdict::Succeeded);
        assert_eq!(fx.read(|v| v.base_for(2)), Some(Oid("i1".into())));
        fx.node("redo", 2);
        fx.contract(
            "redo",
            ExitStatus::Failed,
            Some(("marion/t-redo", "r2")),
            None,
        );
        fx.exit("redo");
        fx.decide(2, StepVerdict::Failed);
        assert_eq!(
            fx.read(|v| v.base_for(3)),
            Some(Oid("i1".into())),
            "a failed step is skipped over"
        );
    }

    /// **A reviewer's findings read as a list**: the summary first, each finding with its
    /// severity, where it is, and what to do, and one marion could not place in the change says
    /// so. A review with no findings has no text.
    #[test]
    fn findings_text_lists_the_summary_then_each_finding() {
        let fx = Fx::new("wf-findings", PLAN_IMPL);
        let finding = |grounded: bool, line: Option<u32>, rec: &str| Finding {
            severity: Severity::High,
            file: "src/a.rs".into(),
            line,
            claim: "it panics".into(),
            evidence: String::new(),
            recommendation: rec.into(),
            grounded,
        };
        fx.contract(
            "rev",
            ExitStatus::Ok,
            None,
            Some(Findings {
                verdict: ModelVerdict::Block,
                summary: "two problems".into(),
                findings: vec![finding(true, Some(7), "check it"), finding(false, None, "")],
                findings_omitted: 0,
            }),
        );
        let task = TaskId("t-rev".into());
        let c: TaskContract = serde_json::from_slice(
            &std::fs::read(fx.project.agent(&id("rev")).contract(&task)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            findings_text(&c).as_deref(),
            Some(
                "two problems\n- [High] src/a.rs:7: it panics — check it\n\
                 - [High, not in the change] src/a.rs: it panics"
            )
        );
        fx.contract(
            "rev",
            ExitStatus::Ok,
            None,
            Some(Findings {
                verdict: ModelVerdict::Allow,
                summary: String::new(),
                findings: vec![],
                findings_omitted: 0,
            }),
        );
        let c: TaskContract = serde_json::from_slice(
            &std::fs::read(fx.project.agent(&id("rev")).contract(&task)).unwrap(),
        )
        .unwrap();
        assert_eq!(findings_text(&c), None);
    }
}
