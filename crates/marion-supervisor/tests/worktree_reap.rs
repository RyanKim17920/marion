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
//! 2. **The branch outlives the worktree.** `marion/<task_id>` is left pointing at `base_commit`,
//!    holding none of the child's work, in the user's own repo, once per spawn — and it makes a
//!    task id single-use, since `git worktree add -b` cannot recreate it.
//! 3. **The child's work survives only as the persisted contract's `diff`** — git holds none of it,
//!    for a created file or a modified one. This is the one item that has already been fixed: it
//!    originally recorded a *created* file's content being destroyed, because `spawn::diff_text`
//!    omitted the intent-to-add pass §6.7 specifies while a *modified tracked* file came through.
//!    The asymmetry was the diagnosis; `diff_text` now implements §6.7's recipe and the two cases
//!    agree. The inverted assertion is kept deliberately, and says so at the test.
//!
//! and one property of the fix, asserted on its own because it is the part that can silently harm a
//! user rather than merely lose data: the diff is derived through a **scratch `GIT_INDEX_FILE`**,
//! so the operator's staged state is untouched.
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

use marion_core::contract::{TaskContract, TaskId, Workspace};
use marion_core::journal::{RecordKind, decode};
use marion_core::paths::ProjectDir;
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::run::{Caller, Env, SpawnRequest, run_spawn};
use marion_testsupport::{fixture_repo, git, judge, on_path, persisted_contracts, scratch};
use serde_json::Value;

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

/// One repo, one canned provider, and the `Env` `run_spawn` takes — held together so a test can
/// spawn into the *same* repository twice.
struct Fixture {
    repo: PathBuf,
    state: PathBuf,
    env: Env,
    /// Held, not dropped: dropping the server closes the port the child talks to.
    _server: CannedServer,
}

fn fixture(root: &Path, patch: &str) -> Fixture {
    let repo = fixture_repo(root);
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: root.join("provider-requests.jsonl"),
        script: Script {
            child_narrative: NARRATIVE.into(),
            child_patch: patch.to_string(),
            ..Script::default()
        },
    })
    .expect("the canned provider binds");

    let env = Env {
        repo: repo.clone(),
        project_dir: ProjectDir::new(&state, &repo),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url: Some(server.base_url()),
        auth: marion_harness::Auth::Canned,
    };
    Fixture {
        repo,
        state,
        env,
        _server: server,
    }
}

fn spawn_one(fx: &Fixture, task_id: &str) -> Result<TaskContract, String> {
    let req = SpawnRequest {
        agent_type: "codex-impl".into(),
        prompt: "Edit the file under src/ and report back through marion.".into(),
        acceptance_criteria: vec!["a file under src/ was edited".into()],
        writable_scope: vec!["src/**".into()],
        timeout_secs: CHILD_TIMEOUT_SECS,
        model: None,
    };
    // A root caller: depth 0, the same thing `marion run` hands the bridge.
    let caller = Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    );
    run_spawn(&fx.env, &req, &TaskId(task_id.into()), &caller).map_err(|e| e.to_string())
}

/// The contract marion wrote to disk — the **uncapped** copy (`cap_for_return` applies to the
/// returned one only), and so the only candidate for a durable record of work the worktree no
/// longer holds.
/// The `expect` on the walk is the shared helper's now, and it is the same decision: a state dir
/// that will not enumerate would otherwise report "no contract", and every claim below about what
/// did or did not survive would be made against a file this test simply failed to look at.
fn persisted_contract(state: &Path, task_id: &str) -> TaskContract {
    let walked = persisted_contracts(state)
        .unwrap_or_else(|e| panic!("{} does not enumerate: {e}", state.display()));
    let wanted = format!("{task_id}.json");
    let found: Vec<(&Path, &Value)> = judge(&walked)
        .into_iter()
        .filter(|(p, _)| p.file_name().is_some_and(|f| f == wanted.as_str()))
        .collect();
    let [(path, value)] = found.as_slice() else {
        let paths: Vec<&Path> = found.iter().map(|(p, _)| *p).collect();
        panic!("expected exactly one persisted contract for {task_id}, found {paths:?}");
    };
    serde_json::from_value((*value).clone())
        .unwrap_or_else(|e| panic!("{} does not parse as a contract: {e}", path.display()))
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
            RecordKind::ContractPersisted(_) => "ContractPersisted",
            RecordKind::PermissionDenied(_) => "PermissionDenied",
            RecordKind::RootChanged(_) => "RootChanged",
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
// 2. What is left in the operator's repository.
// ---------------------------------------------------------------------------------------------

/// **CURRENT BEHAVIOUR, NOT DESIRED.** The branch is created by `spawn::make_worktree` (`git
/// worktree add -b marion/<task_id>`) and deleted by nothing, so one accrues per spawn — pointing
/// at `base_commit`, holding none of the child's work, in the user's own repo.
#[test]
fn the_reap_leaves_an_empty_branch_behind_at_the_base_commit() {
    require_codex();
    let _root = scratch("reap-branch");
    let fx = fixture(&_root, CREATE);
    let contract = spawn_one(&fx, "reap-branch").expect("the child runs");
    let (_, branch) = workspace_of(&contract);
    let comp = contract
        .completion
        .as_ref()
        .expect("a finished run completes");

    let head = git_try(&fx.repo, &["rev-parse", &branch])
        .unwrap_or_else(|e| panic!("the branch is expected to survive the reap, and did not: {e}"));
    assert_eq!(
        head.trim(),
        contract.base_commit.0,
        "CURRENT BEHAVIOUR, NOT DESIRED: `{branch}` survives the reap pointing at the base commit \
         — it advanced nowhere, because the child committed nothing and marion commits nothing on \
         its behalf"
    );
    assert_eq!(
        git(&fx.repo, &["ls-tree", "-r", "--name-only", &branch])
            .lines()
            .collect::<Vec<_>>(),
        vec!["src/keep.txt"],
        "and its tree is the base tree: the branch is not a record of the work, it is residue. The \
         contract meanwhile attests {:?}",
        comp.changed_paths
    );
}

/// The residue is not inert: a second spawn under the same `task_id` collides with the first one's
/// leftover branch. Pinned so a fix is told which behaviour it changed.
#[test]
fn a_second_spawn_of_the_same_task_id_is_refused_by_the_first_ones_leftover_branch() {
    require_codex();
    let _root = scratch("reap-twice");
    let fx = fixture(&_root, CREATE);
    spawn_one(&fx, "reap-twice").expect("the first child runs");
    let branches = git(&fx.repo, &["branch", "--list", "marion/*"]);
    assert!(
        branches.contains("marion/reap-twice"),
        "the first spawn's branch is still there when the second starts: {branches:?}"
    );

    let second = spawn_one(&fx, "reap-twice");
    let e = second.expect_err(
        "CURRENT BEHAVIOUR, NOT DESIRED: `git worktree add -b marion/reap-twice` cannot create a \
         branch that already exists, so the first run's residue must refuse the second — if this \
         now succeeds, the branch is being cleaned up or reused and that is the fix landing",
    );
    assert!(
        e.contains("a branch named 'marion/reap-twice' already exists"),
        "and it is the branch collision that refused it, not some other failure of the second run \
         — the message is git's own, surfaced verbatim through `SpawnError`, and it is the only \
         thing making this diagnosable from a parent's tool result: {e}"
    );
}

// ---------------------------------------------------------------------------------------------
// 3. Whether the child's work survives at all.
// ---------------------------------------------------------------------------------------------

/// **This assertion is inverted from what it said when the file was written, and that is the
/// point.** It used to pin the defect from the first live run: the contract attested to a file the
/// child created while the content existed nowhere, because `spawn::diff_text` omitted §6.7's
/// intent-to-add pass (`git diff <base> HEAD` is empty with nothing committed, and `git diff HEAD`
/// cannot see an untracked file). The reap then took the only copy.
///
/// Everything about the *reap* is unchanged and still asserted below — the worktree is gone, the
/// branch holds the base tree, nothing is stashed, the reflog holds one line. What changed is the
/// last line: the persisted contract now carries the bytes, so §6.7's audit record is
/// self-sufficient and the work is recoverable from it as a patch.
#[test]
fn a_created_files_content_survives_the_reap_in_the_persisted_contracts_diff() {
    require_codex();
    let _root = scratch("reap-created");
    let fx = fixture(&_root, CREATE);
    let contract = spawn_one(&fx, "reap-created").expect("the child runs");
    let (wt, branch) = workspace_of(&contract);
    let comp = contract
        .completion
        .as_ref()
        .expect("a finished run completes");

    // The claim the contract makes.
    assert_eq!(
        comp.changed_paths,
        vec![PathBuf::from("src/marion_m1.txt")],
        "the contract attests that the child created this file"
    );
    assert!(
        comp.result_commits.is_empty(),
        "and that it committed nothing — `result_commits` is the one child-owned field, and this \
         child, like the first live one, never used it. So the diff below is the *only* record of \
         the work: this is not a case where a commit could be fallen back on"
    );

    // Git holds none of it — unchanged by the diff fix, and the reason the contract has to.
    assert!(!wt.exists(), "the worktree is gone");
    assert!(
        !git(&fx.repo, &["ls-tree", "-r", "--name-only", &branch]).contains("marion_m1.txt"),
        "the branch does not hold it"
    );
    assert_eq!(
        git(&fx.repo, &["stash", "list"]),
        "",
        "nothing was stashed on the way out"
    );
    let reflog = git_try(&fx.repo, &["reflog", "show", &branch]).unwrap_or_default();
    assert!(
        reflog.lines().count() <= 1,
        "and the branch's reflog records only its creation, so there is no earlier tip to recover \
         it from: {reflog}"
    );

    // And the contract does.
    let diff = persisted_contract(&fx.state, "reap-created")
        .completion
        .and_then(|c| c.diff)
        .expect(
            "§6.7's diff is `git diff <base_commit>` with intent-to-add for untracked paths, so a \
             created file reaches it. An absent diff here is the original defect returning: the \
             contract would attest to work whose bytes the reap destroyed",
        );
    assert!(
        diff.value.contains("new file mode"),
        "the patch records it as a creation, which is what the intent-to-add pass buys — without \
         it git emits nothing at all for a path it does not track: {}",
        diff.value
    );
    assert!(
        diff.value
            .contains("+marion M1: written by the canned codex child"),
        "and it carries the child's actual bytes, so the file can be reconstructed from the \
         contract alone: {}",
        diff.value
    );
    assert!(
        !diff.truncated,
        "whole, not capped — the persisted copy is uncapped by construction (`cap_for_return` \
         applies to the returned one), so a truncation here would mean something else shortened it"
    );
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

    let diff = marion_supervisor::spawn::diff_text(&wt, &marion_core::contract::Oid(base.clone()))
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

/// The control for the created-file test, and the case that always worked: a modified tracked file
/// survives the reap in the persisted contract's diff. Its value is that the two now agree — before
/// the intent-to-add fix this passed while its neighbour recorded content being destroyed, and that
/// difference was the whole diagnosis.
#[test]
fn a_modified_tracked_files_content_survives_only_as_the_persisted_contracts_diff() {
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
        "keep\n",
        "and the branch still holds the base version, not the child's"
    );

    let diff = persisted_contract(&fx.state, "reap-modified")
        .completion
        .and_then(|c| c.diff)
        .expect("a tracked modification is visible to `git diff <base>` whatever the index holds");
    assert!(
        diff.value
            .contains("keep, edited by the canned codex child"),
        "the child's actual bytes survive in the persisted contract — the sole recovery route for a \
         run whose worktree has been reaped: {}",
        diff.value
    );
    assert!(
        !diff.truncated,
        "and this one is whole, so the comparison with the created-file case is about capture and \
         not about caps"
    );
}
