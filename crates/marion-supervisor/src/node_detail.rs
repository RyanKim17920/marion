//! **`node/get`'s detail**: the facts beside a node's row that a person watching it wants — the
//! task it was given, what it is doing, the tokens it spent, where it works and how it ended.
//!
//! Split in two on purpose. [`inputs`] takes what it needs from the replayed node, and runs under
//! the registry's lock; [`read`] does the file reads — the node's contract and its `events.jsonl`
//! — after the lock is released, so a slow disk or a long stream never holds up the tree.
//!
//! Every read is a view: a missing or unreadable file leaves its field `None` rather than failing
//! the `node/get` it rides on, because the row itself is still true.

use marion_core::agent_type;
use marion_core::contract::{AgentId, TaskContract, TaskId, Workspace};
use marion_core::harness::Harness;
use marion_core::paths::ProjectDir;
use marion_core::proto::result::{CompletionSummary, NodeDetail};
use marion_core::registry::ReplayedNode;
use marion_harness::adapter::adapter_for_type;

/// What [`read`] needs from the replayed node, taken under the registry lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    pub harness: Harness,
    pub acp_agent: Option<String>,
    pub exited: bool,
    /// Its latest contract: the last one persisted, else the one its spawn intent names.
    pub task_id: Option<TaskId>,
    /// Where it was launched, for a node with no contract to say (a root).
    pub launch_workspace: Option<Workspace>,
}

/// [`Inputs`] from a replayed node, or `None` for one whose spawn intent was never read — there is
/// then no harness to read its stream by.
pub fn inputs(node: &ReplayedNode) -> Option<Inputs> {
    let intent = node.intent.as_ref()?;
    Some(Inputs {
        harness: intent.harness,
        acp_agent: agent_type::builtin(&intent.agent_type).and_then(|t| t.acp_agent),
        exited: node.state.is_exited(),
        task_id: node
            .contracts
            .last()
            .map(|c| c.task_id.clone())
            .or_else(|| intent.task_id.clone()),
        launch_workspace: node.launch_workspace.clone(),
    })
}

/// The detail for node `id`, read from its files under `project`.
pub fn read(project: &ProjectDir, id: &AgentId, i: &Inputs) -> NodeDetail {
    let dir = project.agent(id);
    let events = dir.events();
    let contract = i.task_id.as_ref().and_then(|t| {
        let bytes = std::fs::read(dir.contract(t)).ok()?;
        serde_json::from_slice::<TaskContract>(&bytes).ok()
    });
    let usage = adapter_for_type(i.harness, i.acp_agent.as_deref())
        .ok()
        .and_then(|a| a.usage(&crate::activity::all_frames(&events)));
    NodeDetail {
        task: contract.as_ref().map(|c| c.instructions.value.clone()),
        // A peek only while it runs: a finished node's answer is its completion, and a stale
        // "last said" beside `Exited` would read as work still going on.
        activity: (!i.exited).then(|| crate::activity::peek(&events, i.harness)),
        usage,
        workspace: contract
            .as_ref()
            .map(|c| c.workspace.clone())
            .or_else(|| i.launch_workspace.clone()),
        completion: contract
            .and_then(|c| c.completion)
            .map(|c| CompletionSummary {
                status: c.status,
                narrative: c.narrative.map(|n| n.value),
                branch: c.branch,
                commit: c.commit,
                changed_paths: c.changed_paths.len() + c.changed_paths_omitted,
                exit: c.exit.description,
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventSink, EventWriter};
    use crate::spawn::ChildOutcome;
    use marion_core::contract::{Glob, Oid, RepoIdentity};
    use marion_core::encoding::SystemTime;
    use std::time::Duration;

    fn project(name: &str) -> (ProjectDir, AgentId) {
        let dir = marion_testsupport::scratch(name);
        (
            ProjectDir::new(&dir, &dir.join("repo")),
            AgentId("019f-detail".into()),
        )
    }

    fn record_codex_stream(project: &ProjectDir, id: &AgentId) {
        let path = project.agent(id).events();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let s = EventSink::new(
            EventWriter::open_path(&path, id).unwrap(),
            Harness::Codex,
            "unused".into(),
        );
        s.lifecycle(marion_core::event::Lifecycle::Opened);
        for line in include_str!("../../../tests/fixtures/s6/exec-mcp-report.stream.jsonl").lines()
        {
            s.record_line(line);
        }
    }

    fn write_contract(project: &ProjectDir, id: &AgentId, task: &TaskId, landed: bool) {
        let mut c = crate::spawn::build_contract(
            task.clone(),
            AgentId("parent".into()),
            RepoIdentity {
                git_common_dir: None,
                head_branch: None,
            },
            None::<Oid>,
            Workspace::Worktree {
                path: "/wt/t-1".into(),
                branch: "marion/t-1".into(),
            },
            "add a token-bucket limiter",
            &[],
            &[Glob("**".into())],
            &[Glob("**".into())],
            marion_core::encoding::Duration(Duration::from_secs(900)),
            SystemTime(std::time::SystemTime::now()),
            &ChildOutcome {
                exit_code: Some(0),
                ..ChildOutcome::default()
            },
            None,
            None,
            vec![],
            vec![],
        );
        if landed {
            let comp = c
                .completion
                .as_mut()
                .expect("an exited child has a completion");
            comp.branch = Some("marion/t-1".into());
            comp.commit = Some(Oid("0123456789abcdef".into()));
        } else {
            c.completion = None;
        }
        let path = project.agent(id).contract(task);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&c).unwrap()).unwrap();
    }

    fn inputs(exited: bool, task: Option<&TaskId>) -> Inputs {
        Inputs {
            harness: Harness::Codex,
            acp_agent: None,
            exited,
            task_id: task.cloned(),
            launch_workspace: Some(Workspace::SharedCwd {
                path: "/checkout".into(),
            }),
        }
    }

    /// A landed child: its task, its stream's usage, its worktree, and a completion naming the
    /// branch and commit — and no activity peek, because it has finished.
    #[test]
    fn a_finished_child_reports_its_task_usage_workspace_and_completion() {
        let (p, id) = project("detail-finished");
        let task = TaskId("t-1".into());
        record_codex_stream(&p, &id);
        write_contract(&p, &id, &task, true);
        let d = read(&p, &id, &inputs(true, Some(&task)));
        assert_eq!(d.task.as_deref(), Some("add a token-bucket limiter"));
        assert!(d.activity.is_none(), "{d:?}");
        assert!(
            d.usage.is_some(),
            "the codex stream states its usage: {d:?}"
        );
        assert_eq!(
            d.workspace,
            Some(Workspace::Worktree {
                path: "/wt/t-1".into(),
                branch: "marion/t-1".into()
            })
        );
        let c = d.completion.expect("a completion");
        assert_eq!(c.branch.as_deref(), Some("marion/t-1"));
        assert_eq!(c.commit, Some(Oid("0123456789abcdef".into())));
    }

    /// A running node with no contract (a root): the peek, the launch workspace, nothing invented.
    #[test]
    fn a_running_root_reports_its_activity_and_launch_workspace_only() {
        let (p, id) = project("detail-running");
        record_codex_stream(&p, &id);
        let d = read(&p, &id, &inputs(false, None));
        assert!(d.task.is_none() && d.completion.is_none(), "{d:?}");
        assert!(
            d.activity
                .as_deref()
                .unwrap_or("")
                .starts_with("Recent activity"),
            "{d:?}"
        );
        assert_eq!(
            d.workspace,
            Some(Workspace::SharedCwd {
                path: "/checkout".into()
            })
        );
    }

    /// Nothing on disk yet: every field that needs a file is `None`, and `node/get` still answers.
    #[test]
    fn a_node_with_no_files_yet_has_an_empty_detail_not_an_error() {
        let (p, id) = project("detail-empty");
        let task = TaskId("t-9".into());
        let d = read(&p, &id, &inputs(true, Some(&task)));
        assert!(
            d.task.is_none() && d.usage.is_none() && d.completion.is_none(),
            "{d:?}"
        );
    }
}
