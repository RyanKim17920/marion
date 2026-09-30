//! **Workflow runs, served** (`workflow/run`) and **driven**: each step's nodes launched through
//! the one child path, as the operator's contracted nodes carrying their step's seat, and each
//! run stepped by the pure [`marion_core::workflow::next`] wherever it can change — a step node's
//! end, the run's open, a restart. No controller node, no thread or timer per run.

use std::sync::Arc;

use marion_core::contract::{AgentId, Isolation};
use marion_core::journal::{RecordKind, WorkflowClosed, WorkflowOpened, WorkflowStepDecided};
use marion_core::proto::params::WorkflowRunParams;
use marion_core::proto::result::WorkflowRunResult;
use marion_core::workflow::{Next, StepKind, StepVerdict, WorkflowId, WorkflowSeat};

use super::{DEFAULT_SPAWN_TIMEOUT_SECS, RegistryHandle, RpcError, lock, mint_task_id};
use crate::serve::Peer;

impl RegistryHandle {
    /// `workflow/run`: find the workflow, check it, require a repository's to be trusted, snapshot
    /// it into the run, journal the open, and start its first step. Everything that can refuse
    /// refuses before anything is written.
    pub(super) fn workflow_run(
        &self,
        p: &WorkflowRunParams,
        peer: Peer,
    ) -> Result<WorkflowRunResult, RpcError> {
        super::root_spawn_authorized(peer)?;
        let Some(env) = self.spawn_env.clone() else {
            return Err(RpcError::unimplemented(
                "workflow/run",
                "this handle runs no nodes, so it cannot run a workflow's steps.",
                "workflow",
            ));
        };
        super::check_project(&env, &p.repo)?;
        let refuse = |why: String| RpcError::refused(&p.name, why, "workflow");
        let loaded = crate::workflow_file::load(
            &p.repo,
            crate::workflow_file::user_dir().as_deref(),
            &p.name,
        )
        .map_err(|e| refuse(e.to_string()))?;
        let wf = &loaded.workflow;
        if let Some(step) = wf.steps.iter().find(|s| {
            matches!(
                s.kind,
                StepKind::Race { .. } | StepKind::Review { .. } | StepKind::Land { .. }
            )
        }) {
            return Err(refuse(format!(
                "step `{}` is a {} step, which this build does not run yet; agent and parallel \
                 steps run",
                step.id,
                step.kind.word()
            )));
        }
        if let Some(missing) = wf.inputs.iter().find(|i| !p.inputs.contains_key(*i)) {
            return Err(refuse(format!(
                "the workflow needs its input `{missing}`: pass --input {missing}=<text>"
            )));
        }
        if let Some(extra) = p.inputs.keys().find(|k| !wf.inputs.contains(k)) {
            return Err(refuse(format!(
                "`{extra}` is not one of the workflow's inputs ({})",
                wf.inputs.join(", ")
            )));
        }
        let wf_id = crate::run::entropy()
            .map(|e| marion_core::workflow::new_workflow_id(crate::run::unix_millis(), e))
            .map_err(|e| RpcError::internal(format!("marion could not mint a workflow id: {e}")))?;
        let spec = crate::workflow::Spec {
            workflow: loaded.workflow.clone(),
            inputs: p.inputs.clone(),
            repo: p.repo.clone(),
            path: loaded.found.path.clone(),
            digest: loaded.digest.clone(),
        };
        crate::workflow::write_spec(&env.project_dir, &wf_id, &spec).map_err(|e| {
            RpcError::internal(format!(
                "marion could not write the run's spec, so nothing ran: {e}"
            ))
        })?;
        let deadline = wf.budget.wall_secs.map(|s| {
            marion_core::encoding::SystemTime(
                std::time::SystemTime::now() + std::time::Duration::from_secs(s),
            )
        });
        self.journal_append(RecordKind::WorkflowOpened(WorkflowOpened {
            wf_id: wf_id.clone(),
            name: wf.name.clone(),
            digest: loaded.digest.clone(),
            requester: AgentId(marion_core::workflow::OPERATOR.into()),
            base: crate::spawn::head_commit(crate::run::tree_of(&env, &p.repo)).map(|o| o.0),
            deadline,
            budget_tokens: wf.budget.tokens,
        }))
        .map_err(|e| {
            RpcError::internal(format!(
                "marion could not journal the run, so nothing ran: {e}"
            ))
        })?;
        let steps = u8::try_from(wf.steps.len()).unwrap_or(u8::MAX);
        let name = wf.name.clone();
        self.workflows.open(&wf_id, spec);
        self.drive_workflow(&wf_id);
        Ok(WorkflowRunResult { wf_id, name, steps })
    }

    /// **Step a run and do what the step says**, until it has to wait: journal the steps whose
    /// nodes have all ended, skip what an unmet `when` skips, launch the next step, or close the
    /// run. Reads the state afresh from the journal every time, so it is safe to call twice or
    /// after a restart; a step already launched is never launched again.
    pub(crate) fn drive_workflow(&self, wf_id: &WorkflowId) {
        loop {
            let Some(spec) = self.workflows.begin_drive(wf_id) else {
                return;
            };
            self.step_workflow(wf_id, &spec);
            if !self.workflows.end_drive(wf_id) {
                return;
            }
        }
    }

    fn step_workflow(&self, wf_id: &WorkflowId, spec: &crate::workflow::Spec) {
        let Some(env) = self.spawn_env.clone() else {
            return;
        };
        loop {
            self.live.refresh();
            let Some((run, nodes)) = self.live.read(|r| {
                let run = r.tree().workflow(wf_id)?.clone();
                Some((run, r.tree().nodes().to_vec()))
            }) else {
                return;
            };
            let wf = &spec.workflow;
            let (state, earned) = crate::workflow::state(&env.project_dir, wf, &run, &nodes);
            if !earned.is_empty() {
                for (step, verdict) in earned {
                    let ids = run
                        .step_nodes(u8::try_from(step).unwrap_or(u8::MAX), 0)
                        .into_iter()
                        .cloned()
                        .collect();
                    self.decide_step(wf_id, step, verdict, ids);
                }
                continue;
            }
            match marion_core::workflow::next(wf, &state) {
                Next::Wait => return,
                Next::Skip { step } => {
                    self.decide_step(wf_id, step, StepVerdict::Skipped, Vec::new())
                }
                Next::Launch { step } => {
                    if !self.launch_step(wf_id, spec, &env, &run, &nodes, step) {
                        // Nothing started: the step failed where it stood.
                        self.decide_step(wf_id, step, StepVerdict::Failed, Vec::new());
                        continue;
                    }
                    return;
                }
                Next::Close(outcome) => {
                    let result = crate::workflow::result(
                        &env.project_dir,
                        spec,
                        wf_id,
                        &run,
                        &nodes,
                        outcome,
                    );
                    if let Err(e) = crate::workflow::write_result(&env.project_dir, &result) {
                        eprintln!(
                            "marion: could not write workflow {}'s result, left open: {e}",
                            wf_id.0
                        );
                        return;
                    }
                    if let Err(e) = self.journal_now(RecordKind::WorkflowClosed(WorkflowClosed {
                        wf_id: wf_id.clone(),
                        outcome,
                    })) {
                        eprintln!(
                            "marion: could not journal workflow {}'s close: {e}",
                            wf_id.0
                        );
                    }
                    self.workflows.close(wf_id);
                    return;
                }
            }
        }
    }

    fn decide_step(
        &self,
        wf_id: &WorkflowId,
        step: usize,
        verdict: StepVerdict,
        nodes: Vec<AgentId>,
    ) {
        if let Err(e) = self.journal_now(RecordKind::WorkflowStepDecided(WorkflowStepDecided {
            wf_id: wf_id.clone(),
            step: u8::try_from(step).unwrap_or(u8::MAX),
            round: 0,
            verdict,
            nodes,
        })) {
            eprintln!(
                "marion: could not journal workflow {}'s step {step}: {e}",
                wf_id.0
            );
        }
    }

    /// Start step `step`'s nodes; `false` when none started. Each is the operator's contracted
    /// node, in its own worktree, carrying its seat on its intent.
    fn launch_step(
        &self,
        wf_id: &WorkflowId,
        spec: &crate::workflow::Spec,
        env: &crate::run::Env,
        run: &marion_core::registry::ReplayedWorkflow,
        nodes: &[marion_core::registry::ReplayedNode],
        step: usize,
    ) -> bool {
        let Some(me) = self.me.upgrade() else {
            return false;
        };
        let def = &spec.workflow.steps[step];
        let values = crate::workflow::RunValues {
            spec,
            project: &env.project_dir,
            run,
            nodes,
        };
        let (agents, prompt, read_only, verify): (Vec<_>, _, bool, Vec<String>) = match &def.kind {
            StepKind::Agent {
                on,
                prompt,
                read_only,
                verify,
            } => (vec![on.clone()], prompt, *read_only, verify.clone()),
            StepKind::Parallel { on, prompt } => (on.clone(), prompt, true, Vec::new()),
            _ => return false,
        };
        let text = prompt.render(&values);
        let mut started = 0;
        for (part, agent) in agents.iter().enumerate() {
            let seat = WorkflowSeat {
                wf_id: wf_id.clone(),
                step: u8::try_from(step).unwrap_or(u8::MAX),
                round: 0,
                part: u8::try_from(part).unwrap_or(u8::MAX),
            };
            let req = crate::run::SpawnRequest {
                budget: None,
                review: None,
                agent_type: agent.agent_type.clone(),
                prompt: text.clone(),
                repo: spec.repo.clone(),
                acceptance_criteria: vec![],
                verification: verify.clone(),
                race: None,
                writable_scope: vec![],
                timeout_secs: def.timeout_secs.unwrap_or(DEFAULT_SPAWN_TIMEOUT_SECS),
                model: agent.model.clone(),
                isolation: Isolation::Worktree,
                allow_concurrent_writes: false,
                resume: None,
                profile: None,
                read_only,
                workflow: Some(seat),
            };
            let decision = lock(&self.spawn_decision);
            let launched = mint_task_id().and_then(|task_id| {
                self.launch_child(
                    me.clone(),
                    env.clone(),
                    req,
                    task_id,
                    crate::run::Requester::Operator {
                        allow_wider_children: false,
                    },
                    spec.repo.clone(),
                    decision,
                    None,
                )
            });
            match launched {
                Ok(_) => started += 1,
                Err(e) => eprintln!(
                    "marion: workflow {}'s step `{}` could not start {}: {}",
                    wf_id.0,
                    def.id,
                    agent.label(),
                    e.message
                ),
            }
        }
        started > 0
    }

    /// **A restart re-drives every run its predecessor left open**, from each run's own spec.
    pub(super) fn redrive_open_workflows(&self) {
        let Some(env) = self.spawn_env.as_ref() else {
            return;
        };
        let open: Vec<WorkflowId> = self.live.read(|r| {
            r.tree()
                .workflows()
                .iter()
                .filter(|w| w.closed.is_none() && w.opened.is_some())
                .map(|w| w.wf_id.clone())
                .collect()
        });
        for id in open {
            match crate::workflow::read_spec(&env.project_dir, &id) {
                Some(spec) => {
                    self.workflows.open(&id, spec);
                    self.drive_workflow(&id);
                }
                None => eprintln!("marion: workflow {} has no readable spec; left open", id.0),
            }
        }
    }
}

/// A step's own node ended: drive its run.
pub(super) fn after_step_node(handle: &Arc<RegistryHandle>, seat: &WorkflowSeat) {
    handle.drive_workflow(&seat.wf_id);
}
