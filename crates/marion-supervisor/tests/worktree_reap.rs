//! **What a finished child leaves behind in the operator's repository.**
//!
//! Every assertion here is a *measurement of what marion does today*, and several of them pin
//! behaviour that is wrong. Those are labelled `CURRENT BEHAVIOUR, NOT DESIRED` at the assertion,
//! with the message stating what the right answer would look like — so a fix has a starting point,
//! and so this file fails loudly when one lands rather than encoding the defect as correct and
//! quietly outliving it.
//!
//! Three things are measured, all against a real `codex` child driven by the in-process
//! [`CannedServer`]:
//!
//! 1. **The reap is unjournalled.** `run::cleanup` runs `git worktree remove --force` and writes no
//!    record. `RecordKind::ReapIntent`/`ReapConfirmed` are defined in `marion-core` and applied by
//!    `registry::apply`; the supervisor constructs neither, so §7.2's restart resolution has
//!    nothing to resolve.
//! 2. **The child's work is committed onto its branch before the reap.** `marion/<task_id>` gets
//!    one commit holding exactly the contract's `changed_paths` — out-of-scope paths included and
//!    still flagged — the worktree directory is removed, the branch is kept, and the contract
//!    records `branch` and `commit`. A child that changed nothing has nothing to keep: its branch,
//!    still at `base_commit`, is compare-and-deleted with the worktree, so its task id is reusable.
//!    The operator's own branch never moves. Until this landed the reap deleted the
//!    uncommitted work and left the branch empty, so the diff text in the contract was the only copy.
//! 3. **The persisted contract's `diff` still carries the work too**, for a created file or a
//!    modified one, so the audit record is self-sufficient without the branch.
//!
//! and two properties asserted on their own because they are the parts that can silently harm a
//! user rather than merely lose data: the diff is derived through a **scratch `GIT_INDEX_FILE`**,
//! so the operator's staged state is untouched, and the reap removes **only a worktree marion
//! created**, never a `shared-cwd` caller's own linked worktree.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test worktree_reap
//! ```
//!
//! It needs a real `codex` on `PATH` and does not skip when that is missing: §9's rule is that a
//! criterion which quietly passes on a machine that cannot run it is worth less than no criterion.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use marion_core::contract::Isolation;
use marion_core::contract::{TaskContract, TaskId, Workspace};
use marion_core::journal::{RecordKind, decode};
use marion_provider::Script;
use marion_supervisor::run::{Caller, Env, SpawnRequest, run_spawn};
use marion_testsupport::{fixture_repo, git, on_path, persisted_contract, scratch};

mod common;

use common::canned::{CannedFixture as Fixture, canned_fixture};

const CHILD_TIMEOUT_SECS: u64 = 60;
const NARRATIVE: &str = "Edited the worktree and reported back without committing.";

/// The default child: it **creates** a file. Untracked, never committed — the shape the first live
/// M1 hop produced, and the one whose content the reap destroys.
const CREATE: &str = "*** Begin Patch\n*** Add File: src/marion_m1.txt\n\
                      +marion M1: written by the canned codex child\n*** End Patch";

/// The other half of the asymmetry: the child **modifies a file already in `base_commit`**.
const MODIFY: &str = "*** Begin Patch\n*** Update File: src/keep.txt\n@@\n-keep\n\
                      +keep, edited by the canned codex child\n*** End Patch";

/// `git` for the questions whose honest answer may be "there is no such ref".
fn git_try(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git runs");
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn fixture(root: &Path, patch: &str) -> Fixture {
    canned_fixture(
        root,
        fixture_repo(root),
        Script {
            child_narrative: NARRATIVE.into(),
            child_patch: patch.to_string(),
            ..Script::default()
        },
    )
}

fn spawn_one(fx: &Fixture, task_id: &str) -> Result<TaskContract, String> {
    spawn_in(fx, task_id, &fx.repo, Isolation::Worktree)
}

/// [`spawn_one`] with the directory and the isolation chosen by the caller.
fn spawn_in(
    fx: &Fixture,
    task_id: &str,
    repo: &Path,
    isolation: Isolation,
) -> Result<TaskContract, String> {
    let req = SpawnRequest {
        budget: None,
        review: None,
        agent_type: "codex-impl".into(),
        prompt: "Edit the file under src/ and report back through marion.".into(),
        repo: repo.to_path_buf(),
        acceptance_criteria: vec!["a file under src/ was edited".into()],
        verification: vec![],
        writable_scope: vec!["src/**".into()],
        timeout_secs: CHILD_TIMEOUT_SECS,
        model: None,
        isolation,
        allow_concurrent_writes: false,
        resume: None,
        profile: None,
        race: None,
    };
    // A root caller: depth 0, the same thing `marion run` hands the bridge.
    let caller = Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    );
    run_spawn(&fx.env, &req, &TaskId(task_id.into()), &caller).map_err(|e| e.to_string())
}

/// How long to keep looking for a record that is missing on the first read.
///
/// Not a tolerance to absorb a race — nothing here may pass on the second read. It exists to tell
/// **late** from **lost**, because the two are different findings and a bare "not present" cannot
/// distinguish them.
const RECORD_GRACE: Duration = Duration::from_secs(2);

/// Every record kind in the run's journal, in order.
fn journal_kinds(env: &Env) -> Vec<&'static str> {
    kinds_of(&read_journal(env).1)
}

fn read_journal(env: &Env) -> (PathBuf, Vec<u8>) {
    let path = env.project_dir.journal();
    // Not `unwrap_or_default()`: an unreadable journal would make "no reap record" true for the
    // wrong reason, which is the one way the assertion below could pass without looking.
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("the run's journal {} cannot be read: {e}", path.display()));
    (path, bytes)
}

fn kinds_of(bytes: &[u8]) -> Vec<&'static str> {
    bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| {
            decode(l).unwrap_or_else(|| {
                panic!(
                    "a journal line marion wrote does not decode: {:?}",
                    String::from_utf8_lossy(l)
                )
            })
        })
        .map(|r| match r.kind {
            RecordKind::SpawnIntent(_) => "SpawnIntent",
            RecordKind::Spawned(_) => "Spawned",
            RecordKind::SpawnAborted(_) => "SpawnAborted",
            RecordKind::StateChanged(_) => "StateChanged",
            RecordKind::Exited(_) => "Exited",
            RecordKind::ReapIntent(_) => "ReapIntent",
            RecordKind::ReapConfirmed(_) => "ReapConfirmed",
            RecordKind::KillIntent(_) => "KillIntent",
            RecordKind::KillConfirmed(_) => "KillConfirmed",
            RecordKind::CancelRequested(_) => "CancelRequested",
            RecordKind::ContractPersisted(_) => "ContractPersisted",
            RecordKind::PermissionDenied(_) => "PermissionDenied",
            RecordKind::WiderDelegation(_) => "WiderDelegation",
            RecordKind::RootChanged(_) => "RootChanged",
            RecordKind::RootGrantDecided(_) => "RootGrantDecided",
            RecordKind::SessionObserved(_) => "SessionObserved",
            RecordKind::UsageRecorded(_) => "UsageRecorded",
            RecordKind::MessageQueued(_) => "MessageQueued",
            RecordKind::MessageDelivered(_) => "MessageDelivered",
            RecordKind::MessageDropped(_) => "MessageDropped",
            RecordKind::ProfileFailover(_) => "ProfileFailover",
            RecordKind::BudgetCrossed(_) => "BudgetCrossed",
            RecordKind::SupervisorExited(_) => "SupervisorExited",
            RecordKind::RaceOpened(_) => "RaceOpened",
            RecordKind::RaceDecided(_) => "RaceDecided",
        })
        .collect()
}

/// Assert `wanted` is in the journal **on the first read**, and say which of the two failures it is
/// when it is not.
///
/// `run_spawn` has returned by the time this runs, and every record it writes was written by *this*
/// process with one unbuffered `write(2)` — `Journal::append` calls `File::write` directly, and
/// §4.3's group commit governs only the `fsync`, never whether the bytes reached the file. So the
/// page cache already holds them and a read here cannot miss them. Two consequences, both asserted:
///
/// * a record that is **never** found is *lost*, not pending — and since `Journal::record` reports a
///   failed append with a bare `eprintln!` and lets the run continue (`journal.rs:179-183`), a lost
///   record leaves no other trace. The message says where to look.
/// * a record found only on a **later** read is *late*, which for a same-process unbuffered write
///   should be impossible. That is a durability finding in its own right, so it fails too rather
///   than being absorbed as a retry. A test that quietly waited would convert exactly this evidence
///   into a green run.
fn assert_journalled(env: &Env, wanted: &str, context: &str) {
    let (path, bytes) = read_journal(env);
    if kinds_of(&bytes).contains(&wanted) {
        return;
    }
    let deadline = Instant::now() + RECORD_GRACE;
    let started = Instant::now();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        let (_, later) = read_journal(env);
        if kinds_of(&later).contains(&wanted) {
            panic!(
                "{context}: the {wanted} record was absent when `run_spawn` returned and appeared \
                 {:?} later. It is written by this process with one unbuffered `write(2)` before \
                 the call returns, so a read that missed it means the journal's durability story is \
                 weaker than the code claims — a real defect, not a race for this test to wait \
                 out.\njournal: {}\n{}",
                started.elapsed(),
                path.display(),
                String::from_utf8_lossy(&later)
            );
        }
    }
    panic!(
        "{context}: no {wanted} record, and none arrived within {RECORD_GRACE:?}, so it is lost \
         rather than late. `Journal::record` reports a failed append with a bare `eprintln!` and \
         lets the run continue, so check this test's captured stderr for `marion: journal write \
         failed` — that line, not this assertion, would name the cause.\njournal: {} ({} bytes)\n{}",
        path.display(),
        bytes.len(),
        String::from_utf8_lossy(&bytes)
    );
}

/// The worktree path and branch the contract says the child ran in.
fn workspace_of(c: &TaskContract) -> (PathBuf, String) {
    match &c.workspace {
        Workspace::Worktree { path, branch } => (path.clone(), branch.clone()),
        other => panic!("a codex child runs in a worktree, got {other:?}"),
    }
}

fn require_codex() {
    assert!(
        on_path("codex"),
        "this file drives a REAL codex child; put `codex` on PATH"
    );
}

// ---------------------------------------------------------------------------------------------
// 1. The reap happens, and nothing records it.
// ---------------------------------------------------------------------------------------------

/// The worktree really is removed — so every claim below about what survives is a statement about a
/// reaped run, and not about one that simply never cleaned up.
#[test]
fn a_finished_childs_worktree_is_removed_from_disk_and_deregistered() {
    require_codex();
    let _root = scratch("reap-removed");
    let fx = fixture(&_root, CREATE);
    let contract = spawn_one(&fx, "reap-removed").expect("the child runs");
    let (wt, _) = workspace_of(&contract);

    assert!(
        !wt.exists(),
        "`run::cleanup` runs `git worktree remove --force`, so the directory is gone: {}",
        wt.display()
    );
    let listed = git(&fx.repo, &["worktree", "list"]);
    assert_eq!(
        listed.lines().skip(1).count(),
        0,
        "and git no longer lists it as a linked worktree:\n{listed}"
    );
}

/// **The reap removes only a worktree marion created.** A `shared-cwd` child runs in the caller's
/// own directory, and when that directory is itself a *linked* worktree — a root started in a
/// feature worktree — `git worktree remove --force` on it succeeds: git refuses only the main
/// working tree. So the cleanup that follows every spawn deleted the operator's own checkout, with
/// whatever was uncommitted in it, after a child that was told to leave it alone.
#[test]
fn a_shared_cwd_child_leaves_the_linked_worktree_it_ran_in() {
    require_codex();
    let _root = scratch("reap-shared-linked");
    let fx = fixture(&_root, CREATE);
    let linked = _root.join("linked");
    git(
        &fx.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            &linked.to_string_lossy(),
        ],
    );
    std::fs::write(
        linked.join("operator-notes.txt"),
        "the operator's own work\n",
    )
    .unwrap();

    spawn_in(&fx, "reap-shared-linked", &linked, Isolation::SharedCwd).expect("the child runs");

    assert_eq!(
        std::fs::read_to_string(linked.join("operator-notes.txt"))
            .ok()
            .as_deref(),
        Some("the operator's own work\n"),
        "the caller's linked worktree, and the uncommitted file in it, survive a shared-cwd spawn"
    );
    assert!(
        git(&fx.repo, &["worktree", "list"]).contains("[feature]"),
        "and git still lists it as a worktree"
    );
}

/// **CURRENT BEHAVIOUR, NOT DESIRED.** §4.3 makes the reap a barrier transition and §7.2 resolves an
/// *unconfirmed* reap intent on restart — both presuppose an intent is written. Nothing writes one.
#[test]
fn the_reap_reaches_the_journal_as_no_record_at_all() {
    require_codex();
    let _root = scratch("reap-unjournalled");
    let fx = fixture(&_root, CREATE);
    spawn_one(&fx, "reap-unjournalled").expect("the child runs");
    // The run really was journalled. Without this, "no reap record" could just mean "no journal",
    // and the assertion below would hold on a run that recorded nothing whatsoever. Each of the
    // four goes through `assert_journalled`, which tells a lost record from a late one —
    // `ContractPersisted` is the one that matters, being a NON-barrier kind (with `StateChanged`
    // and `PermissionDenied`) and the last record the run writes.
    for expected in ["SpawnIntent", "Spawned", "Exited", "ContractPersisted"] {
        assert_journalled(
            &fx.env,
            expected,
            "a run that spawned a child records its whole lifecycle",
        );
    }
    let kinds = journal_kinds(&fx.env);
    assert!(
        !kinds.iter().any(|k| k.starts_with("Reap")),
        "CURRENT BEHAVIOUR, NOT DESIRED: marion removed this child's worktree and recorded nothing \
         about it. A journal carrying the reap would hold a ReapIntent before the `git worktree \
         remove` and a ReapConfirmed after it — §4.3's intent/act/confirm barrier, which §7.2's \
         restart resolution reads. When that lands, invert this assertion: {kinds:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// 2. What is left in the operator's repository: the child's work, on its own branch.
// ---------------------------------------------------------------------------------------------

/// A child that edits nothing: its patch rewrites `src/keep.txt` with the bytes it already holds.
const NOOP: &str =
    "*** Begin Patch\n*** Update File: src/keep.txt\n@@\n-keep\n+keep\n*** End Patch";

/// A child that writes **outside** `writable_scope` (`src/**`), at the worktree's root.
const OUTSIDE: &str = "*** Begin Patch\n*** Add File: outside.txt\n\
                       +written outside the child's writable scope\n*** End Patch";

/// The base commit a worktree child was cut from.
fn base_of(c: &TaskContract) -> String {
    c.base_commit
        .clone()
        .expect("a worktree child always has a base commit")
        .0
}

/// The paths one commit changed relative to its parent, sorted.
fn paths_in_commit(repo: &Path, commit: &str) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = git(
        repo,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "--no-renames",
            "-r",
            commit,
        ],
    )
    .lines()
    .map(PathBuf::from)
    .collect();
    paths.sort();
    paths
}

/// What `git commit` in `repo` would use as the author: the operator's configured name, else
/// marion's own. Asked of git rather than assumed, so the test holds on any machine.
fn expected_author(repo: &Path) -> String {
    let get = |k: &str| {
        git_try(repo, &["config", "--get", k])
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    match (get("user.name"), get("user.email")) {
        (Some(name), Some(email)) => format!("{name} <{email}>"),
        _ => "marion <marion@localhost>".to_string(),
    }
}

/// **The child's work outlives its worktree, as one commit on `marion/<task_id>`.**
///
/// Before this, the reap ran `git worktree remove --force` over a tree holding the child's
/// uncommitted edits, and the branch was left at `base_commit` — so the only copy of the work was
/// the diff text inside the contract, while the parent told the human the file had changed. Now
/// marion commits exactly the paths the contract names onto the child's own branch first, removes
/// only the directory, and records where the work is. It never touches the operator's branch.
#[test]
fn a_childs_changes_are_committed_onto_its_branch_before_the_reap() {
    require_codex();
    let _root = scratch("reap-branch");
    let fx = fixture(&_root, CREATE);
    let config_before = std::fs::read(fx.repo.join(".git/config")).unwrap();
    let contract = spawn_one(&fx, "reap-branch").expect("the child runs");
    let (wt, branch) = workspace_of(&contract);
    let base = base_of(&contract);
    let comp = contract
        .completion
        .as_ref()
        .expect("a finished run completes");

    assert!(
        !wt.exists(),
        "the worktree is still reaped: {}",
        wt.display()
    );
    // Named for the node's short id and its task's first words, never the operator's branch.
    assert!(
        branch.starts_with("marion/") && branch.ends_with("-edit-the-file-under-src"),
        "{branch}"
    );
    let tip = git(&fx.repo, &["rev-parse", &branch]).trim().to_string();
    assert_eq!(
        git(
            &fx.repo,
            &["rev-list", "--count", &format!("{base}..{branch}")]
        )
        .trim(),
        "1",
        "the branch is the base commit plus exactly one commit"
    );
    assert_eq!(
        git(&fx.repo, &["rev-parse", &format!("{branch}^")]).trim(),
        base
    );
    assert_eq!(
        paths_in_commit(&fx.repo, &tip),
        comp.changed_paths,
        "the commit holds exactly the paths the contract attests, no more and no fewer"
    );
    assert_eq!(
        git(&fx.repo, &["show", &format!("{branch}:src/marion_m1.txt")]),
        "marion M1: written by the canned codex child\n",
        "and the child's actual bytes"
    );
    assert_eq!(
        git(
            &fx.repo,
            &["log", "-1", "--format=%an <%ae>|%cn <%ce>", &branch]
        )
        .trim(),
        format!("{0}|{0}", expected_author(&fx.repo)),
        "authored and committed as the operator's configured identity, or marion's own"
    );
    let subject = git(&fx.repo, &["log", "-1", "--format=%s", &branch]);
    assert_eq!(
        subject.trim(),
        format!("marion: codex-impl reap-bra: {NARRATIVE}"),
        "the subject names the agent type, the task and the child's own first line"
    );

    // The contract says where the work is, and so does the copy on disk.
    assert_eq!(comp.branch.as_deref(), Some(branch.as_str()));
    assert_eq!(
        comp.commit.as_ref().map(|c| c.0.as_str()),
        Some(tip.as_str())
    );
    let persisted = persisted_contract(&fx.state, "reap-branch")
        .completion
        .expect("the persisted contract completes");
    assert_eq!(persisted.branch, comp.branch);
    assert_eq!(persisted.commit, comp.commit);
    assert_eq!(
        comp.landed_line().as_deref(),
        Some(
            format!(
                "changes on branch {branch} ({}); merge with: git merge --no-ff {branch}",
                &tip[..12]
            )
            .as_str()
        ),
        "the one line a parent shows the human"
    );

    // And nothing of the operator's moved.
    assert_eq!(
        git(&fx.repo, &["rev-parse", "main"]).trim(),
        base,
        "marion never merges: the operator's branch is where it was"
    );
    assert!(
        !fx.repo.join("src/marion_m1.txt").exists(),
        "and the operator's checkout does not hold the child's file"
    );
    assert_eq!(
        std::fs::read(fx.repo.join(".git/config")).unwrap(),
        config_before,
        "and the repository's config is byte-for-byte what it was: the identity is passed per \
         command, never written"
    );
}

/// **A child that changed nothing leaves nothing**: no commit, no branch or commit on the contract —
/// there is nothing to merge — and its task branch, still at the base commit, is reaped with its
/// worktree.
#[test]
fn a_child_that_changed_nothing_leaves_no_branch_behind() {
    require_codex();
    let _root = scratch("reap-noop");
    let fx = fixture(&_root, NOOP);
    let contract = spawn_one(&fx, "reap-noop").expect("the child runs");
    let (wt, branch) = workspace_of(&contract);
    let comp = contract
        .completion
        .as_ref()
        .expect("a finished run completes");

    assert!(comp.changed_paths.is_empty(), "{:?}", comp.changed_paths);
    assert!(!wt.exists(), "the worktree is reaped");
    assert!(
        git_try(
            &fx.repo,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")]
        )
        .is_err(),
        "no commit was made for a child with no changes, so its branch was residue and is gone"
    );
    assert_eq!((&comp.branch, &comp.commit), (&None, &None));
    assert_eq!(comp.landed_line(), None);
}

/// **An out-of-scope write is committed too, and still flagged.** Dropping it would destroy the
/// evidence `scope_violations` points at; committing it keeps the contract and the branch saying
/// the same thing, and the parent decides whether to merge.
#[test]
fn an_out_of_scope_write_is_committed_and_flagged() {
    require_codex();
    let _root = scratch("reap-outside");
    let fx = fixture(&_root, OUTSIDE);
    let contract = spawn_one(&fx, "reap-outside").expect("the child runs");
    let (_, branch) = workspace_of(&contract);
    let comp = contract
        .completion
        .as_ref()
        .expect("a finished run completes");

    assert!(comp.scope_enforced);
    assert_eq!(comp.scope_violations, vec![PathBuf::from("outside.txt")]);
    let tip = git(&fx.repo, &["rev-parse", &branch]).trim().to_string();
    assert_eq!(
        paths_in_commit(&fx.repo, &tip),
        vec![PathBuf::from("outside.txt")]
    );
    assert_eq!(
        comp.commit.as_ref().map(|c| c.0.as_str()),
        Some(tip.as_str())
    );
}

/// **A task id whose first run changed nothing can be used again**: that run's branch was still
/// at its base, so cleanup deleted it, and the second `git worktree add -b marion/<task_id>` has
/// nothing to collide with. It used to: the residue made every task id single-use.
#[test]
fn a_second_spawn_can_reuse_a_task_id_whose_first_run_changed_nothing() {
    require_codex();
    let _root = scratch("reap-twice-noop");
    let fx = fixture(&_root, NOOP);
    spawn_one(&fx, "reap-twice-noop").expect("the first child runs");
    spawn_one(&fx, "reap-twice-noop")
        .expect("the first run's unchanged branch was reaped, so the task id is free");
    let branches = git(&fx.repo, &["branch", "--list", "marion/*"]);
    assert!(
        branches.trim().is_empty(),
        "the second run reaps its own unchanged branch too: {branches:?}"
    );
}

/// The residue is not inert, and it is not handed on: a second spawn under the same `task_id` is a
/// second node, on a branch of its own, and the first one's branch keeps the first child's work
/// exactly where it was.
#[test]
fn a_second_spawn_of_the_same_task_id_gets_its_own_branch_and_leaves_the_first_ones() {
    require_codex();
    let _root = scratch("reap-twice");
    let fx = fixture(&_root, CREATE);
    let first = spawn_one(&fx, "reap-twice").expect("the first child runs");
    let (_, first_branch) = workspace_of(&first);
    let first_tip = git(&fx.repo, &["rev-parse", &first_branch]);
    assert_ne!(
        first_tip.trim(),
        base_of(&first),
        "the first child's branch holds its work, so the reap kept it"
    );

    let second = spawn_one(&fx, "reap-twice").expect("the second child runs");
    let (_, second_branch) = workspace_of(&second);
    assert_ne!(second_branch, first_branch);
    assert_eq!(
        git(&fx.repo, &["rev-parse", &first_branch]),
        first_tip,
        "the first child's work was not handed to the second"
    );
}

// ---------------------------------------------------------------------------------------------
// 3. The contract's own copy of the work.
// ---------------------------------------------------------------------------------------------

/// **The branch is not the only copy.** §6.7's diff still carries a created file's bytes, through
/// the intent-to-add pass on a scratch index, so the audit record stays self-sufficient even for a
/// reader that never looks at the branch. This test used to pin the opposite: before that pass, a
/// created file reached neither git nor the contract once the worktree was reaped.
#[test]
fn a_created_files_content_survives_the_reap_in_the_persisted_contracts_diff() {
    require_codex();
    let _root = scratch("reap-created");
    let fx = fixture(&_root, CREATE);
    let contract = spawn_one(&fx, "reap-created").expect("the child runs");
    let comp = contract
        .completion
        .as_ref()
        .expect("a finished run completes");

    assert_eq!(
        comp.changed_paths,
        vec![PathBuf::from("src/marion_m1.txt")],
        "the contract attests that the child created this file"
    );
    assert!(
        comp.result_commits.is_empty(),
        "and that the child committed nothing itself — `result_commits` is the child's own field, \
         and marion's commit is recorded in `commit`, not added to it"
    );
    let diff = persisted_contract(&fx.state, "reap-created")
        .completion
        .and_then(|c| c.diff)
        .expect(
            "§6.7's diff is `git diff <base_commit>` with intent-to-add for untracked paths, so a \
             created file reaches it",
        );
    assert!(
        diff.value.contains("new file mode"),
        "the patch records it as a creation: {}",
        diff.value
    );
    assert!(
        diff.value
            .contains("+marion M1: written by the canned codex child"),
        "and it carries the child's actual bytes: {}",
        diff.value
    );
    assert!(!diff.truncated, "whole, not capped");
}

// ---------------------------------------------------------------------------------------------
// 4. The scratch index: deriving the diff must not touch what the user has staged.
// ---------------------------------------------------------------------------------------------

/// `diff_text` called directly, on a worktree that has **staged** state of its own.
///
/// The end-to-end test below shows the operator's repository index surviving a spawn, but a child
/// runs in a linked worktree, and a linked worktree has its own index — so that run would pass even
/// if `diff_text` scribbled all over the index it actually operates on. This test closes that hole
/// by pointing `diff_text` at a workspace whose index state is known, and comparing it byte for
/// byte across the call. It is also the case §6.7 is really about: `isolation` defaults to
/// `shared-cwd`, where the workspace *is* the user's own checkout.
#[test]
fn deriving_the_diff_leaves_the_workspaces_own_index_byte_for_byte_unchanged() {
    let _root = scratch("reap-scratch-index");
    let repo = fixture_repo(&_root);
    let wt = _root.join("wt");
    let base = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "probe",
            &wt.to_string_lossy(),
            &base,
        ],
    );

    // Deliberate index state: one staged modification, plus an untracked file the diff must pick
    // up. If the intent-to-add pass ran in *this* index, the untracked file would join the staged
    // set and the user's next `git commit` would carry a file marion only ever read.
    std::fs::write(wt.join("src/keep.txt"), "keep, staged by the user\n").unwrap();
    git(&wt, &["add", "src/keep.txt"]);
    std::fs::write(wt.join("src/untracked.txt"), "created, never staged\n").unwrap();

    let before_staged = git(&wt, &["diff", "--cached", "--name-only"]);
    let before_status = git(&wt, &["status", "--porcelain"]);
    let index_before =
        std::fs::read(repo.join(".git/worktrees/wt/index")).expect("the worktree index");

    let diff = marion_supervisor::spawn::diff_text(
        marion_supervisor::spawn::Tree::Child {
            wt: &wt,
            repo: &repo,
        },
        &_root.join("scratch"),
        &marion_core::contract::Oid(base.clone()),
    )
    .expect("the diff derives");

    assert!(
        diff.contains("+created, never staged"),
        "the untracked file's content reaches the patch — otherwise this test proves only that a \
         no-op touches nothing: {diff}"
    );
    assert_eq!(
        git(&wt, &["diff", "--cached", "--name-only"]),
        before_staged,
        "the staged set is exactly what it was: `git add -N` ran against the scratch index, so \
         `src/untracked.txt` did not join it"
    );
    assert_eq!(
        git(&wt, &["status", "--porcelain"]),
        before_status,
        "and nothing else moved between git's columns either"
    );
    assert_eq!(
        std::fs::read(repo.join(".git/worktrees/wt/index")).expect("the worktree index"),
        index_before,
        "the index file is unchanged byte for byte — the strongest form of the claim, and the one \
         that would catch a refresh that happened to leave `status` reading the same"
    );
    // Checking that no index file *remains* in the worktree would prove nothing: `ScratchIndex`
    // removes it on `Drop`, so it is gone by the time this test could look, wherever it was
    // written. The observable harm of writing it inside the workspace is that the intent-to-add
    // pass finds it — `ls-files --others` runs after it is created — and the instrument lands in
    // the patch as a new file. That is what this asserts, and it is what caught the mutation.
    assert!(
        !diff.contains("marion-diff-index"),
        "the scratch index must not be written inside the workspace, or the diff reports the \
         instrument that produced it: {diff}"
    );
}

/// The same property end to end: a file the operator staged in their own repository before a spawn
/// is still staged, and still unchanged, after one.
#[test]
fn a_spawn_leaves_the_operators_staged_state_alone() {
    require_codex();
    let _root = scratch("reap-operator-index");
    let fx = fixture(&_root, CREATE);

    // The operator stages something and walks away, exactly as they might while a child runs.
    std::fs::write(fx.repo.join("src/staged.txt"), "staged by the operator\n").unwrap();
    git(&fx.repo, &["add", "src/staged.txt"]);
    let staged_before = git(&fx.repo, &["diff", "--cached", "--name-only"]);
    assert_eq!(
        staged_before.trim(),
        "src/staged.txt",
        "the fixture really did stage something, so the assertion below is not vacuous"
    );

    spawn_one(&fx, "reap-operator-index").expect("the child runs");

    assert_eq!(
        git(&fx.repo, &["diff", "--cached", "--name-only"]),
        staged_before,
        "the operator's staged set survives the spawn untouched"
    );
    assert_eq!(
        std::fs::read_to_string(fx.repo.join("src/staged.txt")).unwrap(),
        "staged by the operator\n",
        "and so do its contents"
    );
}

/// The control for the created-file test: a **modified** tracked file reaches both the branch and
/// the persisted diff, so the two cases agree on where the work lands.
#[test]
fn a_modified_tracked_files_content_survives_on_the_branch_and_in_the_persisted_diff() {
    require_codex();
    let _root = scratch("reap-modified");
    let fx = fixture(&_root, MODIFY);
    let contract = spawn_one(&fx, "reap-modified").expect("the child runs");
    let (wt, branch) = workspace_of(&contract);
    let comp = contract
        .completion
        .as_ref()
        .expect("a finished run completes");

    assert_eq!(comp.changed_paths, vec![PathBuf::from("src/keep.txt")]);
    assert!(!wt.exists(), "the worktree is gone here too");
    assert_eq!(
        git(&fx.repo, &["show", &format!("{branch}:src/keep.txt")]),
        "keep, edited by the canned codex child\n",
        "and the branch holds the child's version"
    );
    assert_eq!(
        git(&fx.repo, &["show", "main:src/keep.txt"]),
        "keep\n",
        "while the operator's branch still holds the base version"
    );

    let diff = persisted_contract(&fx.state, "reap-modified")
        .completion
        .and_then(|c| c.diff)
        .expect("a tracked modification is visible to `git diff <base>` whatever the index holds");
    assert!(
        diff.value
            .contains("keep, edited by the canned codex child"),
        "the child's bytes are in the persisted contract as well: {}",
        diff.value
    );
    assert!(!diff.truncated);
}
