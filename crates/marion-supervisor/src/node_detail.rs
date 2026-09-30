//! **`node/get`'s detail**: the facts beside a node's row that a person watching it wants — the
//! task it was given, what it is doing, the tokens it spent, where it works and how it ended.
//!
//! Split in two on purpose. [`inputs`] takes what it needs from the replayed node, and runs under
//! the registry's lock; [`read`] does the file reads — the node's contract and its `events.jsonl`
//! — after the lock is released, so a slow disk or a long stream never holds up the tree.
//!
//! Every read is a view: a missing or unreadable file leaves its field `None` rather than failing
//! the `node/get` it rides on, because the row itself is still true.

use crate::spending::Spent;
use marion_core::contract::Oid;
use marion_core::contract::{AgentId, TaskContract, TaskId, Workspace};
use marion_core::harness::Harness;
use marion_core::journal::{self, MessageSource, RecordKind};
use marion_core::paths::ProjectDir;
use marion_core::proto::params::ActivityCursor;
use marion_core::proto::result::{CompletionSummary, DiffStat, MessageLine, NodeDetail, TaskSent};
use marion_core::registry::ReplayedNode;
use std::collections::HashMap;
use std::path::Path;

/// What [`read`] needs from the replayed node, taken under the registry lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    pub harness: Harness,
    pub exited: bool,
    /// Its latest contract: the last one persisted, else the one its spawn intent names.
    pub task_id: Option<TaskId>,
    /// Where it was launched, for a node with no contract to say (a root).
    pub launch_workspace: Option<Workspace>,
    /// What its ended runs spent, as the journal recorded them (`ReplayedNode::usage`, `turns`).
    pub recorded: Spent,
}

/// [`Inputs`] from a replayed node, or `None` for one whose spawn intent was never read — there is
/// then no harness to read its stream by.
pub fn inputs(node: &ReplayedNode) -> Option<Inputs> {
    let intent = node.intent.as_ref()?;
    Some(Inputs {
        harness: intent.harness,
        exited: node.state.is_exited(),
        task_id: node
            .contracts
            .last()
            .map(|c| c.task_id.clone())
            .or_else(|| intent.task_id.clone()),
        launch_workspace: node.launch_workspace.clone(),
        recorded: Spent::recorded(node),
    })
}

/// The detail for node `id`, read from its files under `project`, with a page of its activity
/// stream from `cursor` when one was asked for.
///
/// Usage and per-turn spend are [`crate::spending::shown`]: the journal's record, with `live` — the
/// run in progress its sink published — on top while the node runs. Neither reads its stream.
pub fn read(
    project: &ProjectDir,
    id: &AgentId,
    i: &Inputs,
    cursor: Option<ActivityCursor>,
    live: Option<&Spent>,
) -> NodeDetail {
    let dir = project.agent(id);
    let events = dir.events();
    let contract = i.task_id.as_ref().and_then(|t| {
        let bytes = std::fs::read(dir.contract(t)).ok()?;
        serde_json::from_slice::<TaskContract>(&bytes).ok()
    });
    let spent = crate::spending::shown(i.exited, &i.recorded, live);
    let workspace = contract
        .as_ref()
        .map(|c| c.workspace.clone())
        .or_else(|| i.launch_workspace.clone());
    NodeDetail {
        // A child's task is its contract's; a root has none (§9), so its kept prompt.
        task: contract.as_ref().map(task_sent).or_else(|| root_task(&dir)),
        messages: messages(&project.journal(), id),
        stream: cursor.map(|c| {
            let root = workspace.as_ref().map(|w| w.path().as_path());
            crate::activity::page(&events, i.harness, c, root)
        }),
        usage: spent.usage,
        workspace,
        completion: contract.and_then(|contract| {
            let diff = landed_diff(&contract);
            contract.completion.map(|c| CompletionSummary {
                status: c.status,
                narrative: c.narrative.map(|n| n.value),
                branch: c.branch,
                commit: c.commit,
                changed_paths: c.changed_paths.len() + c.changed_paths_omitted,
                exit: c.exit.description,
                diff,
            })
        }),
        turns: spent.turns,
    }
}

/// What a contract's landed branch changed against the commit it started from, when it landed
/// one: `git diff --shortstat <base> <commit>` in the repository's common dir, where the branch's
/// objects live even once its worktree is gone.
pub(crate) fn landed_diff(c: &TaskContract) -> Option<DiffStat> {
    let completion = c.completion.as_ref()?;
    completion.branch.as_ref()?;
    diff_stat(
        c.repo.git_common_dir.as_ref()?,
        c.base_commit.as_ref()?,
        completion.commit.as_ref()?,
    )
}

/// `git diff --shortstat base commit` against `git_dir`, **once per pair**: both are commits, so
/// the answer never changes, and a watcher asking about a landed node every second must not run
/// git every second. `None` when git cannot say (a missing object, no git on `PATH`).
fn diff_stat(git_dir: &Path, base: &Oid, commit: &Oid) -> Option<DiffStat> {
    type Key = (std::path::PathBuf, String, String);
    type Cache = std::collections::HashMap<Key, Option<DiffStat>>;
    static CACHE: std::sync::Mutex<Option<Cache>> = std::sync::Mutex::new(None);
    let key = (git_dir.to_path_buf(), base.0.clone(), commit.0.clone());
    if let Some(hit) = CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .get(&key)
    {
        return *hit;
    }
    let mut cmd = std::process::Command::new("git");
    cmd.arg("--git-dir")
        .arg(git_dir)
        .args([
            "-c",
            "core.fsmonitor=false",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--shortstat",
        ])
        .arg(&base.0)
        .arg(&commit.0)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    // Through the spawn gate, as every process this supervisor starts is.
    let stat = crate::spawn_receive_gate::SPAWN_RECEIVE_GATE
        .spawn(&mut cmd)
        .and_then(|child| child.wait_with_output())
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| parse_shortstat(&String::from_utf8_lossy(&out.stdout)));
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .insert(key, stat);
    stat
}

/// `--shortstat`'s line — ` 3 files changed, 84 insertions(+), 12 deletions(-)`, any clause of
/// which may be absent — as its three numbers; an empty answer is no change. `None` for a line
/// that is not one.
fn parse_shortstat(out: &str) -> Option<DiffStat> {
    let line = out.trim();
    let mut stat = DiffStat::default();
    if line.is_empty() {
        return Some(stat);
    }
    for clause in line.split(',') {
        let mut words = clause.split_whitespace();
        let n: u32 = words.next()?.parse().ok()?;
        match words.next()? {
            w if w.starts_with("file") => stat.files = n,
            w if w.starts_with("insertion") => stat.added = n,
            w if w.starts_with("deletion") => stat.removed = n,
            _ => return None,
        }
    }
    Some(stat)
}

/// Keep the prompt a root was launched with beside its stream, owner-only, for [`read`] to show
/// as its task. Best-effort by the same policy as its event record: a viewer never fails a run, so
/// a write that fails is said on stderr and the run goes on.
pub fn persist_root_prompt(dir: &marion_core::paths::AgentDir, prompt: &str) {
    use std::io::Write;
    let path = dir.prompt();
    let written = crate::private_fs::create(&path).and_then(|mut f| f.write_all(prompt.as_bytes()));
    if let Err(e) = written {
        eprintln!(
            "marion: cannot keep the root's prompt at {}: {e}",
            path.display()
        );
    }
}

/// A root's kept prompt as its task, when there is one.
fn root_task(dir: &marion_core::paths::AgentDir) -> Option<TaskSent> {
    let prompt = std::fs::read_to_string(dir.prompt()).ok()?;
    Some(task_of(&prompt, Vec::new(), Vec::new()))
}

/// The contract's task as the node received it.
pub(crate) fn task_sent(c: &TaskContract) -> TaskSent {
    task_of(
        &c.instructions.value,
        c.acceptance_criteria
            .iter()
            .map(|a| a.value.clone())
            .collect(),
        c.verification
            .iter()
            .map(crate::bridge::command_line)
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
    fold_messages(&bytes, |a| a == id)
        .remove(id)
        .unwrap_or_default()
}

/// [`messages`] for every node at once, from journal bytes already read: one pass, for a reader
/// that wants the whole tree's steers (`marion export`) rather than one node's.
pub fn messages_by_node(journal: &[u8]) -> HashMap<AgentId, Vec<MessageLine>> {
    fold_messages(journal, |_| true)
}

/// The delivery records of every node `keep` accepts, folded into their message lines.
fn fold_messages(
    journal: &[u8],
    keep: impl Fn(&AgentId) -> bool,
) -> HashMap<AgentId, Vec<MessageLine>> {
    let mut by_node: HashMap<AgentId, Vec<(String, MessageLine)>> = HashMap::new();
    let resolve = |by_node: &mut HashMap<AgentId, Vec<(String, MessageLine)>>,
                   id: &AgentId,
                   mid: &str,
                   outcome: String| {
        if let Some((_, m)) = by_node
            .get_mut(id)
            .and_then(|out| out.iter_mut().find(|(k, _)| k == mid))
        {
            m.outcome = outcome;
        }
    };
    for line in journal.split(|b| *b == b'\n') {
        let Some(record) = journal::decode(line) else {
            continue;
        };
        match record.kind {
            RecordKind::MessageQueued(q) if keep(&q.agent_id) => {
                let from = match q.source {
                    MessageSource::Operator => "operator".to_string(),
                    MessageSource::Ancestor(a) => format!("ancestor {}", a.0),
                    MessageSource::ChildEnded { child, status, .. } => {
                        format!("child {} ended {status}", child.0)
                    }
                    MessageSource::ReportRequested => "marion, asking for its report".to_string(),
                    MessageSource::RaceDecided { race_id, .. } => {
                        format!("marion: race {} decided", race_id.0)
                    }
                    MessageSource::BudgetWarning { spent, limit, .. } => {
                        format!("marion: budget {spent}/{limit}")
                    }
                };
                by_node.entry(q.agent_id).or_default().push((
                    q.message_id,
                    MessageLine {
                        at: crate::activity::rfc3339(record.ts),
                        from,
                        len: q.len,
                        outcome: "queued".into(),
                    },
                ));
            }
            RecordKind::MessageDelivered(d) if keep(&d.agent_id) => {
                resolve(
                    &mut by_node,
                    &d.agent_id,
                    &d.message_id,
                    format!("delivered via {}", d.via),
                );
            }
            RecordKind::MessageDropped(d) if keep(&d.agent_id) => {
                resolve(
                    &mut by_node,
                    &d.agent_id,
                    &d.message_id,
                    format!("dropped: {}", d.reason),
                );
            }
            _ => {}
        }
    }
    by_node
        .into_iter()
        .map(|(id, lines)| (id, lines.into_iter().map(|(_, m)| m).collect()))
        .collect()
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
        // codex's own stream since S36: app-server's frames, as its driver records them.
        for line in marion_testsupport::app_server_capture("p4-items.jsonl").lines() {
            s.record_line(line);
        }
    }

    fn write_contract(project: &ProjectDir, id: &AgentId, task: &TaskId, landed: bool) {
        let mut c = crate::spawn::build_contract(
            task.clone(),
            AgentId("parent".into()),
            marion_core::Harness::Codex,
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
            exited,
            task_id: task.cloned(),
            launch_workspace: Some(Workspace::SharedCwd {
                path: "/checkout".into(),
            }),
            recorded: Spent::default(),
        }
    }

    /// **Usage is the journal's record, with the run in progress on top only while the node runs**
    /// — and never a read of its stream: none exists here. An ended node shows its record alone,
    /// whatever a stale live entry says; a node with neither claims nothing.
    #[test]
    fn usage_is_the_record_with_the_live_run_on_top_while_the_node_runs() {
        let (p, id) = project("detail-recorded");
        let tokens = |input| marion_core::contract::TokenUsage {
            input,
            ..Default::default()
        };
        let recorded = Spent {
            usage: Some(tokens(90)),
            turns: vec![90],
        };
        let live = Spent {
            usage: Some(tokens(5)),
            turns: vec![5],
        };
        let with = |exited| Inputs {
            recorded: recorded.clone(),
            ..inputs(exited, None)
        };
        let ended = read(&p, &id, &with(true), None, Some(&live));
        assert_eq!((ended.usage, ended.turns), (Some(tokens(90)), vec![90]));
        let running = read(&p, &id, &with(false), None, Some(&live));
        assert_eq!(
            (running.usage, running.turns),
            (Some(tokens(95)), vec![90, 5])
        );
        assert_eq!(read(&p, &id, &inputs(true, None), None, None).usage, None);
    }

    /// A landed child: its task, its recorded usage, its worktree, and a completion naming the
    /// branch and commit — and no activity peek, because it has finished.
    #[test]
    fn a_finished_child_reports_its_task_usage_workspace_and_completion() {
        let (p, id) = project("detail-finished");
        let task = TaskId("t-1".into());
        record_codex_stream(&p, &id);
        write_contract(&p, &id, &task, true);
        let recorded = Spent {
            usage: Some(marion_core::contract::TokenUsage {
                input: 30,
                output: 7,
                ..Default::default()
            }),
            turns: vec![12, 25],
        };
        let d = read(
            &p,
            &id,
            &Inputs {
                recorded: recorded.clone(),
                ..inputs(true, Some(&task))
            },
            None,
            None,
        );
        let t = d.task.clone().expect("a child has a task");
        assert_eq!(t.prompt, "add a token-bucket limiter");
        assert_eq!(
            t.appended.as_deref(),
            Some(crate::bridge::REPORT_INSTRUCTION)
        );
        assert_eq!(t.acceptance, ["tests pass"]);
        assert_eq!(t.verification, ["cargo test -q"]);
        assert!(d.stream.is_none(), "no page was asked for: {:?}", d.stream);
        assert_eq!((d.usage, d.turns.clone()), (recorded.usage, recorded.turns));
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

    /// **A check reads the same live and ended.** A running node shows its verification lines as
    /// the parent wrote them; once it ends they are read back off the contract, where each is
    /// marion's `sh -c` wrapper — and must come back as the same line, not `sh -c <script>`. A
    /// command that was not wrapped is shell-quoted, so the line is still one a shell runs as-is.
    #[test]
    fn a_check_reads_the_same_before_and_after_the_node_ends() {
        let lines = vec![
            "python3 -m unittest -q".to_string(),
            "test \"$(cat out)\" = 'a b'".to_string(),
        ];
        let live = task_of("p", Vec::new(), lines.clone());
        let ended: Vec<String> = crate::run::verification_commands(&lines, Path::new("/wt"), None)
            .iter()
            .map(crate::bridge::command_line)
            .collect();
        assert_eq!(ended, live.verification);
        let unwrapped = marion_core::contract::Command {
            program: "grep".into(),
            args: vec!["-q".into(), "top N".into(), "wordfreq.py".into()],
            cwd: "/wt".into(),
            timeout: marion_core::encoding::Duration(Duration::from_secs(300)),
        };
        assert_eq!(
            crate::bridge::command_line(&unwrapped),
            "grep -q \"top N\" wordfreq.py"
        );
    }

    /// **A root's task is the prompt it was launched with**, kept 0600 beside its stream because
    /// a root has no contract to hold it; the prompt is shown, marion's own suffix split off as on
    /// a child.
    #[test]
    fn a_root_shows_the_prompt_it_was_launched_with() {
        let (p, id) = project("detail-root-prompt");
        let dir = p.agent(&id);
        persist_root_prompt(&dir, "list the limiter files");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.prompt())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the prompt is the operator's words: owner-only"
        );
        let d = read(&p, &id, &inputs(false, None), None, None);
        let t = d.task.expect("a root with a kept prompt has a task");
        assert_eq!(t.prompt, "list the limiter files");
        assert!(t.acceptance.is_empty() && t.verification.is_empty());
    }

    /// **A landed branch's diff stat comes from git**, base to landed commit, and each of
    /// `--shortstat`'s three clauses may be absent.
    #[test]
    fn a_landed_branch_is_measured_against_the_commit_it_started_from() {
        assert_eq!(
            parse_shortstat(" 3 files changed, 84 insertions(+), 12 deletions(-)\n"),
            Some(DiffStat {
                added: 84,
                removed: 12,
                files: 3
            })
        );
        assert_eq!(
            parse_shortstat(" 1 file changed, 1 deletion(-)"),
            Some(DiffStat {
                added: 0,
                removed: 1,
                files: 1
            })
        );
        assert_eq!(
            parse_shortstat(""),
            Some(DiffStat::default()),
            "no change is zero, said"
        );
        assert_eq!(parse_shortstat("fatal: bad revision"), None);

        let dir = marion_testsupport::scratch("detail-diffstat");
        let repo = marion_testsupport::fixture_repo(&dir);
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        let base = git(&["rev-parse", "HEAD"]);
        std::fs::write(repo.join("src/keep.txt"), "kept\nand more\n").unwrap();
        std::fs::write(repo.join("src/new.txt"), "new\n").unwrap();
        git(&["add", "-A"]);
        git(&[
            "-c",
            "user.email=m@example.invalid",
            "-c",
            "user.name=m",
            "commit",
            "-qm",
            "landed",
        ]);
        let landed = git(&["rev-parse", "HEAD"]);
        let common = repo.join(".git");
        assert_eq!(
            diff_stat(&common, &Oid(base.clone()), &Oid(landed.clone())),
            Some(DiffStat {
                added: 3,
                removed: 1,
                files: 2
            })
        );
        assert_eq!(
            diff_stat(&common, &Oid(base), &Oid("0".repeat(40))),
            None,
            "a commit git cannot find is no stat, not zero"
        );
    }

    /// **A landed branch's own `.gitattributes` runs no textconv program**: a child can commit
    /// `* diff=<driver>` naming any driver the operator's config defines, and the stat marion
    /// computes for the watcher must not run it over the child's content. git 2.50's `--shortstat`
    /// was measured not to consult textconv at all; `--no-textconv` makes that marion's decision
    /// rather than git's default, and this pins it.
    #[test]
    fn the_diff_stat_runs_no_textconv_driver_the_landed_branch_names() {
        let dir = marion_testsupport::scratch("detail-diffstat-textconv");
        let repo = marion_testsupport::fixture_repo(&dir);
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        let marker = dir.join("textconv-ran");
        let driver = dir.join("textconv.sh");
        std::fs::write(
            &driver,
            format!("#!/bin/sh\ntouch '{}'\ncat \"$1\"\n", marker.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(&["config", "diff.probe.textconv", &driver.to_string_lossy()]);
        let base = git(&["rev-parse", "HEAD"]);
        std::fs::write(repo.join(".gitattributes"), "*.txt diff=probe\n").unwrap();
        std::fs::write(repo.join("src/keep.txt"), "changed by the child\n").unwrap();
        git(&["add", "-A"]);
        git(&[
            "-c",
            "user.email=m@example.invalid",
            "-c",
            "user.name=m",
            "commit",
            "-qm",
            "landed",
        ]);
        let landed = git(&["rev-parse", "HEAD"]);
        let stat = diff_stat(&repo.join(".git"), &Oid(base), &Oid(landed));
        assert!(stat.is_some(), "the stat is still computed");
        assert!(
            !marker.exists(),
            "marion ran a textconv driver the branch named"
        );
    }

    /// A running node with no contract (a root): its stream page, the launch workspace, nothing
    /// invented.
    #[test]
    fn a_running_root_reports_its_stream_and_launch_workspace_only() {
        let (p, id) = project("detail-running");
        record_codex_stream(&p, &id);
        let d = read(
            &p,
            &id,
            &inputs(false, None),
            Some(ActivityCursor::Tail),
            None,
        );
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
            None,
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
                note: None,
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
        let all = messages_by_node(&std::fs::read(p.journal()).unwrap());
        assert_eq!(
            all.get(&id),
            Some(&m),
            "one pass agrees with the one-node read"
        );
        assert_eq!(all.get(&other).map(Vec::len), Some(1));
    }
}
