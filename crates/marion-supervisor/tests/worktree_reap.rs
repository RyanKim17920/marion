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
//! 3. **A created file is destroyed by the reap.** The contract truthfully attests
//!    `changed_paths: ["src/marion_m1.txt"]` while the content exists nowhere: not in the branch,
//!    not in the worktree, not in a stash or reflog, and not in the contract's own `diff`. A
//!    *modified tracked* file survives, in the persisted contract's diff alone. The asymmetry is
//!    `spawn::diff_text`, which omits the intent-to-add pass §6.7 specifies.
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

use marion_core::contract::{TaskContract, TaskId, Workspace};
use marion_core::journal::{RecordKind, decode};
use marion_core::paths::ProjectDir;
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::run::{Caller, Env, SpawnRequest, run_spawn};

const CHILD_TIMEOUT_SECS: u64 = 60;
const NARRATIVE: &str = "Edited the worktree and reported back without committing.";

/// The default child: it **creates** a file. Untracked, never committed — the shape the first live
/// M1 hop produced, and the one whose content the reap destroys.
const CREATE: &str = "*** Begin Patch\n*** Add File: src/marion_m1.txt\n\
                      +marion M1: written by the canned codex child\n*** End Patch";

/// The other half of the asymmetry: the child **modifies a file already in `base_commit`**.
const MODIFY: &str = "*** Begin Patch\n*** Update File: src/keep.txt\n@@\n-keep\n\
                      +keep, edited by the canned codex child\n*** End Patch";

/// A scratch dir that removes itself.
///
/// `Drop`, and not a `remove_dir_all` at the end of each test: a failing assertion unwinds straight
/// past any trailing cleanup, so an explicit call leaks on exactly the runs that fail — the ones a
/// developer re-runs most. `Drop` catches those, plus every `?` and early return.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        // Ignored: the dir may already be gone, and a cleanup failure must not mask the test's own
        // verdict.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

/// So the guard can be handed to anything taking `impl AsRef<Path>` — `Deref` alone does not
/// satisfy that bound, and its omission is what broke two targets when this guard first landed.
impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// Bind the returned guard for the whole test — `scratch("x").join("y")` drops the dir at the end
/// of that statement, deleting it out from under the test. Callers bind `_root`, never a bare `_`,
/// which would drop it there and then rather than at the end of the test.
fn scratch(name: &str) -> Scratch {
    let p = std::env::temp_dir().join(format!("marion-reap-{name}-{}", std::process::id()));
    // Removed on the way *in* as well: a run killed hard enough to skip `Drop` leaves a dir behind,
    // and pids recycle, so a later run can inherit that exact name.
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    Scratch(p.canonicalize().expect("scratch dir canonicalises"))
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

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

/// A repository for the child to worktree. Its own, not marion's: the run writes to it.
fn fixture_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/keep.txt"), "keep\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main", "."]);
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=marion@example.invalid",
            "-c",
            "user.name=marion",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    repo
}

fn on_path(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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
fn persisted_contract(state: &Path, task_id: &str) -> TaskContract {
    fn find(dir: &Path, name: &str, out: &mut Vec<PathBuf>) {
        // `expect`, not a silent skip: a state dir that will not enumerate would report "no
        // contract", and every claim below about what did or did not survive would be made against
        // a file this test simply failed to look at.
        for e in std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("{} does not enumerate: {e}", dir.display()))
            .flatten()
        {
            let p = e.path();
            if p.is_dir() {
                find(&p, name, out);
            } else if p.file_name().is_some_and(|f| f == name)
                && p.parent().is_some_and(|d| d.ends_with("contracts"))
            {
                out.push(p);
            }
        }
    }
    let mut found = Vec::new();
    find(state, &format!("{task_id}.json"), &mut found);
    let [path] = found.as_slice() else {
        panic!("expected exactly one persisted contract for {task_id}, found {found:?}");
    };
    let bytes =
        std::fs::read(path).unwrap_or_else(|e| panic!("{} cannot be read: {e}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("{} does not parse as a contract: {e}", path.display()))
}

/// Every record kind in the run's journal, in order.
fn journal_kinds(env: &Env) -> Vec<&'static str> {
    let path = env.project_dir.journal();
    // Not `unwrap_or_default()`: an unreadable journal would make "no reap record" true for the
    // wrong reason, which is the one way the assertion below could pass without looking.
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("the run's journal {} cannot be read: {e}", path.display()));
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
        })
        .collect()
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
    let _root = scratch("removed");
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
    let _root = scratch("unjournalled");
    let fx = fixture(&_root, CREATE);
    spawn_one(&fx, "reap-unjournalled").expect("the child runs");
    let kinds = journal_kinds(&fx.env);

    // The run really was journalled. Without this, "no reap record" could just mean "no journal",
    // and the assertion below would hold on a run that recorded nothing whatsoever.
    for expected in ["SpawnIntent", "Spawned", "Exited", "ContractPersisted"] {
        assert!(
            kinds.contains(&expected),
            "no {expected} record in a run that spawned a child: {kinds:?}"
        );
    }
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
    let _root = scratch("branch");
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
    let _root = scratch("twice");
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

/// **CURRENT BEHAVIOUR, NOT DESIRED — the defect from the first live run.**
///
/// The contract attests to a file the child created. After the reap its *content* exists nowhere:
/// the worktree is gone, the branch holds the base tree, nothing was stashed, and `spawn::diff_text`
/// produced no patch because it omits the intent-to-add pass §6.7 specifies (`git diff <base> HEAD`
/// is empty with nothing committed, and `git diff HEAD` cannot see an untracked file). So §6.7's
/// audit record truthfully describes work that can be neither recovered nor reviewed.
#[test]
fn a_created_files_content_is_destroyed_by_the_reap_though_the_contract_attests_to_it() {
    require_codex();
    let _root = scratch("created");
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
         child, like the first live one, never used it"
    );

    // Every place the content could have survived.
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
    let diff = persisted_contract(&fx.state, "reap-created")
        .completion
        .and_then(|c| c.diff);
    assert!(
        diff.is_none(),
        "CURRENT BEHAVIOUR, NOT DESIRED: the persisted contract is the last place this content \
         could live, and its `diff` is empty for a *created* file. §6.7 specifies the diff as `git \
         diff <base_commit>` with intent-to-add for untracked paths against a scratch \
         GIT_INDEX_FILE; `spawn::diff_text` does neither, so the one artefact that could have \
         carried these bytes does not. When the intent-to-add pass lands this becomes a `Some` \
         holding the new file, and this assertion is the one to invert. Got: {diff:?}"
    );
}

/// The other half of the asymmetry, and the reason the defect above is about *untracked* paths
/// rather than about reaping in general: a modified tracked file **does** survive the reap — in the
/// persisted contract's diff, and nowhere else.
#[test]
fn a_modified_tracked_files_content_survives_only_as_the_persisted_contracts_diff() {
    require_codex();
    let _root = scratch("modified");
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
        .expect("a tracked modification reaches `git diff HEAD`, so this diff is captured");
    assert!(
        diff.value
            .contains("keep, edited by the canned codex child"),
        "the child's actual bytes survive in the persisted contract — the sole recovery route for a \
         run whose worktree has been reaped, and the one that fails for a created file: {}",
        diff.value
    );
    assert!(
        !diff.truncated,
        "and this one is whole, so the contrast with the created-file case is about capture and \
         not about caps"
    );
}
