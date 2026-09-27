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
use marion_core::journal::{self, MessageSource, RecordKind};
use marion_core::paths::ProjectDir;
use marion_core::proto::params::ActivityCursor;
use marion_core::proto::result::{CompletionSummary, MessageLine, NodeDetail, TaskSent};
use marion_core::registry::ReplayedNode;
use marion_harness::adapter::adapter_for_type;
use std::path::Path;

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

/// The detail for node `id`, read from its files under `project`, with a page of its activity
/// stream from `cursor` when one was asked for.
pub fn read(
    project: &ProjectDir,
    id: &AgentId,
    i: &Inputs,
    cursor: Option<ActivityCursor>,
) -> NodeDetail {
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
        task: contract.as_ref().map(task_sent),
        messages: messages(&project.journal(), id),
        stream: cursor.map(|c| crate::activity::page(&events, i.harness, c)),
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

/// The contract's task as the node received it.
fn task_sent(c: &TaskContract) -> TaskSent {
    task_of(
        &c.instructions.value,
        c.acceptance_criteria
            .iter()
            .map(|a| a.value.clone())
            .collect(),
        c.verification
            .iter()
            .map(|v| {
                std::iter::once(&v.program)
                    .chain(&v.args)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect(),
    )
}

/// A task from the prompt as delivered, with marion's appended report instruction split off so a
/// reader can tell the operator's words from marion's.
pub fn task_of(delivered: &str, acceptance: Vec<String>, verification: Vec<String>) -> TaskSent {
    let suffix = format!("\n\n{}", crate::bridge::REPORT_INSTRUCTION);
    let (prompt, appended) = match delivered.strip_suffix(&suffix) {
        Some(head) => (
            head.to_string(),
            Some(crate::bridge::REPORT_INSTRUCTION.to_string()),
        ),
        None => (delivered.to_string(), None),
    };
    TaskSent {
        prompt,
        appended,
        acceptance,
        verification,
    }
}

/// The messages queued for `id`, oldest first, each with what came of it — read from the
/// journal's delivery records, which carry a length and a digest and never the text.
fn messages(journal: &Path, id: &AgentId) -> Vec<MessageLine> {
    let Ok(bytes) = std::fs::read(journal) else {
        return Vec::new();
    };
    let mut out: Vec<(String, MessageLine)> = Vec::new();
    for line in bytes.split(|b| *b == b'\n') {
        let Some(record) = journal::decode(line) else {
            continue;
        };
        let resolve = |out: &mut Vec<(String, MessageLine)>, mid: &str, outcome: String| {
            if let Some((_, m)) = out.iter_mut().find(|(k, _)| k == mid) {
                m.outcome = outcome;
            }
        };
        match record.kind {
            RecordKind::MessageQueued(q) if &q.agent_id == id => {
                let from = match q.source {
                    MessageSource::Operator => "operator".to_string(),
                    MessageSource::Ancestor(a) => format!("ancestor {}", a.0),
                    MessageSource::ChildEnded { child, status, .. } => {
                        format!("child {} ended {status}", child.0)
                    }
                };
                out.push((
                    q.message_id,
                    MessageLine {
                        at: crate::activity::rfc3339(record.ts),
                        from,
                        len: q.len,
                        outcome: "queued".into(),
                    },
                ));
            }
            RecordKind::MessageDelivered(d) if &d.agent_id == id => {
                resolve(&mut out, &d.message_id, format!("delivered via {}", d.via));
            }
            RecordKind::MessageDropped(d) if &d.agent_id == id => {
                resolve(&mut out, &d.message_id, format!("dropped: {}", d.reason));
            }
            _ => {}
        }
    }
    out.into_iter().map(|(_, m)| m).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventSink, EventWriter};
    use crate::spawn::ChildOutcome;
    use marion_core::contract::{Glob, Oid, RepoIdentity};
    use marion_core::encoding::SystemTime;
    use marion_core::journal::{JournalRecord, MessageDelivered, MessageDropped, MessageQueued};
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
            &format!(
                "add a token-bucket limiter\n\n{}",
                crate::bridge::REPORT_INSTRUCTION
            ),
            &["tests pass".to_string()],
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
            vec![marion_core::contract::Command {
                program: "cargo".into(),
                args: vec!["test".into(), "-q".into()],
                cwd: "/wt/t-1".into(),
                timeout: marion_core::encoding::Duration(Duration::from_secs(300)),
            }],
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
        let d = read(&p, &id, &inputs(true, Some(&task)), None);
        let t = d.task.clone().expect("a child has a task");
        assert_eq!(t.prompt, "add a token-bucket limiter");
        assert_eq!(
            t.appended.as_deref(),
            Some(crate::bridge::REPORT_INSTRUCTION)
        );
        assert_eq!(t.acceptance, ["tests pass"]);
        assert_eq!(t.verification, ["cargo test -q"]);
        assert!(d.stream.is_none(), "no page was asked for: {:?}", d.stream);
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

    /// A running node with no contract (a root): its stream page, the launch workspace, nothing
    /// invented.
    #[test]
    fn a_running_root_reports_its_stream_and_launch_workspace_only() {
        let (p, id) = project("detail-running");
        record_codex_stream(&p, &id);
        let d = read(&p, &id, &inputs(false, None), Some(ActivityCursor::Tail));
        assert!(d.task.is_none() && d.completion.is_none(), "{d:?}");
        let page = d.stream.clone().expect("a page was asked for");
        assert!(page.unread.is_none() && !page.lines.is_empty(), "{page:?}");
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
        let d = read(
            &p,
            &id,
            &inputs(true, Some(&task)),
            Some(ActivityCursor::Tail),
        );
        assert!(
            d.task.is_none() && d.usage.is_none() && d.completion.is_none(),
            "{d:?}"
        );
        assert_eq!(
            d.stream,
            Some(Default::default()),
            "no file is an empty page"
        );
    }

    /// Steers read from the journal: who, when, how long, what came of each — and only this node's.
    #[test]
    fn queued_messages_are_listed_with_their_outcomes_and_never_their_text() {
        let (p, id) = project("detail-messages");
        let other = AgentId("someone-else".into());
        let kinds = vec![
            RecordKind::MessageQueued(MessageQueued {
                agent_id: id.clone(),
                message_id: "m-1".into(),
                source: MessageSource::Operator,
                len: 41,
                sha256: "00".into(),
            }),
            RecordKind::MessageQueued(MessageQueued {
                agent_id: other.clone(),
                message_id: "m-x".into(),
                source: MessageSource::Operator,
                len: 1,
                sha256: "00".into(),
            }),
            RecordKind::MessageQueued(MessageQueued {
                agent_id: id.clone(),
                message_id: "m-2".into(),
                source: MessageSource::Ancestor(AgentId("root".into())),
                len: 12,
                sha256: "00".into(),
            }),
            RecordKind::MessageDelivered(MessageDelivered {
                agent_id: id.clone(),
                message_id: "m-1".into(),
                via: "turn".into(),
            }),
            RecordKind::MessageDropped(MessageDropped {
                agent_id: id.clone(),
                message_id: "m-2".into(),
                reason: "node ended".into(),
            }),
        ];
        let mut bytes = Vec::new();
        for (seq, kind) in kinds.into_iter().enumerate() {
            let record = JournalRecord {
                writer: marion_core::journal::WriterId("w".into()),
                seq: seq as u64,
                ts: SystemTime::from_unix_millis(1_790_000_000_000 + seq as u64),
                mono_ns: 0,
                provenance: marion_core::ir::Provenance::marion(),
                src_seq: None,
                kind,
            };
            bytes.extend(journal::encode(&record).unwrap());
        }
        std::fs::create_dir_all(p.journal().parent().unwrap()).unwrap();
        std::fs::write(p.journal(), bytes).unwrap();
        let m = messages(&p.journal(), &id);
        let got: Vec<(&str, u32, &str)> = m
            .iter()
            .map(|l| (l.from.as_str(), l.len, l.outcome.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("operator", 41, "delivered via turn"),
                ("ancestor root", 12, "dropped: node ended")
            ]
        );
        assert!(m[0].at.ends_with('Z'), "{}", m[0].at);
    }
}
