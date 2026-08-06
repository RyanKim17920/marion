//! **The root change record** (design §9, `marion_core::root_change`), instrument and record both.
//!
//! A root writes in the operator's own repository, so the three things that decide whether its
//! change record is worth anything are *what the delta is measured against*, *what measuring costs
//! the operator*, and *whether the record survives the run's own ending*. The first two are
//! properties of `spawn::TreeSnapshot` and need no process; the third can only be asserted through
//! a real `marion run`, so the second half of this file drives the binary against a stub harness.
//!
//! Every test here is a **negative control** in the sense the design uses: it is built so that it
//! cannot be satisfied by failing to look, and each one was watched red — with the break named in
//! its own doc comment — before it was watched green.
//!
//! # Why the harness is a stub
//!
//! `launch_only_root.rs`'s reason, unchanged: what is under test is *marion's* record of what
//! happened to a directory, and a stub lets the directory's history be the input — including the
//! two histories no real CLI produces on demand (a root that writes and then hangs forever, and one
//! that writes and never reaches marion's bridge). codex is the harness because it is `LaunchOnly`,
//! which is the path where the wall-clock kill lives, and the kill is the exit path a record is
//! most easily lost on.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use marion_core::journal::{RecordKind, decode};
use marion_core::root_change::{RootChange, RootChanged, RootDelta, RootObservation};
use marion_supervisor::run::run_bounded;
use marion_supervisor::spawn::TreeSnapshot;
use marion_testsupport::{fixture_repo, git, scratch};

/// Generous. The bound exists so a hung `marion` fails the suite loudly instead of wedging it —
/// `launch_only_root.rs`'s `RUN_BOUND`, and the same reasoning.
const RUN_BOUND: Duration = Duration::from_secs(60);

/// Files under a directory, counted. The witness NC-3 rests on, so it is a real walk and not a
/// `read_dir` of the top level: git's loose objects live two levels down in `objects/ab/cdef…`.
fn files_under(dir: &Path) -> usize {
    let mut n = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            n += files_under(&p);
        } else {
            n += 1;
        }
    }
    n
}

/// **NC-2 — the dirty-repo control, which is what pins the base point.**
///
/// The operator has uncommitted work when the root launches. If the diff base were `HEAD` — the
/// obvious simplification, and the one this test exists to keep out — that work would be attributed
/// to the root: `changed_paths` would carry `operator.txt`, and an audit record would name a file
/// the agent never touched. That is `8a69f22`'s failure class with the sign flipped, and it is
/// worse than no record, because it is a record that is wrong.
///
/// **Watched red, both halves separately.** Replacing `pre` with `base` in the path assertion
/// yields `["agent.txt", "operator.txt", "src/keep.txt"]` against the expected
/// `["agent.txt", "src/keep.txt"]`; doing it in the patch assertion alone yields a diff carrying
/// `+operator's line`. Two independent reds, because a single one could be satisfied by a base
/// point that is wrong in only one of the two ways.
#[test]
fn the_base_point_is_the_tree_at_launch_and_not_head() {
    let dir = scratch("root-change-dirty");
    let repo = fixture_repo(&dir);
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();

    // The operator's own work in progress, both shapes: an untracked file and a modified tracked
    // one. A path-list base point can subtract the first and gets the second wrong.
    std::fs::write(repo.join("operator.txt"), "half an edit\n").unwrap();
    std::fs::write(repo.join("src/keep.txt"), "keep\noperator's line\n").unwrap();

    let snap = TreeSnapshot::open(&repo, &agent).expect("the fixture is a git worktree");
    let base = snap.head(&repo).expect("the fixture has one commit");
    let pre = snap.take(&repo).unwrap();

    // …and now the root runs, touching one new file and appending to the file the operator was
    // already editing.
    std::fs::write(repo.join("agent.txt"), "the agent wrote this\n").unwrap();
    std::fs::write(
        repo.join("src/keep.txt"),
        "keep\noperator's line\nthe agent's line\n",
    )
    .unwrap();

    let post = snap.take(&repo).unwrap();
    let changed = snap.changed_paths(&repo, &pre, &post).unwrap();
    assert_eq!(
        changed,
        vec![Path::new("agent.txt"), Path::new("src/keep.txt")]
            .into_iter()
            .map(Path::to_path_buf)
            .collect::<Vec<_>>(),
        "only what changed while the root ran; `operator.txt` was already there at t0"
    );

    // The whole reason a *tree* is the base and a path list is not: the file both of them touched
    // yields only the root's hunk.
    let patch = snap.diff(&repo, &pre, &post).unwrap();
    assert!(
        patch.contains("+the agent's line"),
        "the root's own hunk must be in the patch:\n{patch}"
    );
    assert!(
        !patch.contains("+operator's line"),
        "the operator's uncommitted line was subtracted exactly, not merely counted:\n{patch}"
    );

    // `dirty_at_launch`, the number the record carries so a reader can see the subtraction was made
    // rather than assume it. Both of the operator's paths, and neither of the root's.
    let dirty = snap.changed_paths(&repo, &base, &pre).unwrap();
    assert_eq!(
        dirty,
        vec![Path::new("operator.txt"), Path::new("src/keep.txt")]
            .into_iter()
            .map(Path::to_path_buf)
            .collect::<Vec<_>>(),
        "measured against HEAD, which is what `base_commit` is for and the diff base is not"
    );
    assert!(
        !dirty.contains(&Path::new("agent.txt").to_path_buf()),
        "the root had not run yet"
    );
}

/// The other half, so NC-2 cannot pass by always reporting the same list: a root that writes
/// nothing produces an empty delta and an empty patch, in a repo that was dirty the whole time.
#[test]
fn a_dirty_repo_where_nothing_moved_still_reports_nothing_moved() {
    let dir = scratch("root-change-quiet");
    let repo = fixture_repo(&dir);
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(repo.join("operator.txt"), "half an edit\n").unwrap();

    let snap = TreeSnapshot::open(&repo, &agent).unwrap();
    let pre = snap.take(&repo).unwrap();
    let post = snap.take(&repo).unwrap();
    assert_eq!(pre.0, post.0, "the same tree twice is the same object name");
    assert!(snap.changed_paths(&repo, &pre, &post).unwrap().is_empty());
    assert!(snap.diff(&repo, &pre, &post).unwrap().is_empty());
}

/// **The instrument's blind spot, and the one number that says how big it is** (§11 item 26).
///
/// `git add -A .` respects `.gitignore`, so an ignored write moves no tree at all: the two
/// snapshots are the same object name, the path list is empty, the patch is empty. That is the
/// whole hole, asserted here on the instrument rather than argued in prose — and beside it the
/// count that lets a reader tell this reading from a genuinely quiet run.
///
/// Watched red before `ignored_entries` existed: the first three assertions passed and there was
/// nothing to ask the fourth question of.
#[test]
fn an_ignored_write_moves_no_tree_and_the_count_is_what_says_so() {
    let dir = scratch("root-change-ignored");
    let repo = fixture_repo(&dir);
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(repo.join(".gitignore"), ".env\n").unwrap();

    let snap = TreeSnapshot::open(&repo, &agent).unwrap();
    let pre = snap.take(&repo).unwrap();
    assert_eq!(
        snap.ignored_entries(&repo).unwrap(),
        0,
        "nothing is ignored yet, so an empty delta here would be exhaustive"
    );

    std::fs::write(repo.join(".env"), "SECRET=1\n").unwrap();
    let post = snap.take(&repo).unwrap();
    assert_eq!(
        pre.0, post.0,
        "the hole itself: git cannot see an ignored write, so the tree is unmoved"
    );
    assert!(snap.changed_paths(&repo, &pre, &post).unwrap().is_empty());
    assert!(snap.diff(&repo, &pre, &post).unwrap().is_empty());
    assert_eq!(
        snap.ignored_entries(&repo).unwrap(),
        1,
        "…and this is the only thing that distinguishes that empty delta from a quiet run"
    );
}

/// **NC-3 — marion does not mutate the repository it is measuring.**
///
/// Five witnesses, and the load-bearing one is the object count. `git add -A` and `git write-tree`
/// create blobs and trees; without `GIT_OBJECT_DIRECTORY` they land in the operator's own
/// `.git/objects`, where nothing breaks and nothing says so — the quiet kind of damage that is
/// only ever found by counting. `.git/index` is the second: writing it would permanently change
/// what `git status`, `git diff`, `git stash` and `git commit -a` do for the user, which is the
/// property `git_indexed` already states for the child path.
///
/// Watched red by pointing `GIT_OBJECT_DIRECTORY` at the repo's own store — which is exactly what
/// dropping the variable does: `.git/objects` goes from 4 files to 6, and the agent dir stays
/// empty. Two objects, on a two-file fixture; on a real repository it is one per changed blob plus
/// one per tree level, every run, forever.
#[test]
fn taking_two_snapshots_writes_nothing_into_the_operators_own_git_dir() {
    let dir = scratch("root-change-nonmutation");
    let repo = fixture_repo(&dir);
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();

    let git_objects = repo.join(".git/objects");
    let git_index = repo.join(".git/index");
    let objects_before = files_under(&git_objects);
    let index_before = std::fs::read(&git_index).unwrap();
    let head_before = git(&repo, &["rev-parse", "HEAD"]);
    let refs_before = git(&repo, &["for-each-ref"]);
    let status_before = git(&repo, &["status", "--porcelain"]);

    let snap = TreeSnapshot::open(&repo, &agent).unwrap();
    let pre = snap.take(&repo).unwrap();
    std::fs::write(repo.join("agent.txt"), "the agent wrote this\n").unwrap();
    let post = snap.take(&repo).unwrap();
    let _ = snap.changed_paths(&repo, &pre, &post).unwrap();
    let _ = snap.diff(&repo, &pre, &post).unwrap();

    assert_eq!(
        files_under(&git_objects),
        objects_before,
        "every blob and tree marion created must be in the agent dir; \
         `.git/objects` growing is the failure GIT_OBJECT_DIRECTORY exists to prevent, and it is \
         silent — nothing breaks, so only a count finds it"
    );
    assert_eq!(
        std::fs::read(&git_index).unwrap(),
        index_before,
        "the operator's index is byte-identical: marion used a copy, and writing the real one \
         changes what `git status` and `git commit -a` do for the user"
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git(&repo, &["for-each-ref"]), refs_before, "no new refs");
    assert_eq!(
        git(&repo, &["status", "--porcelain"]),
        format!("{status_before}?? agent.txt\n"),
        "the working tree differs by the root's write and by nothing else"
    );

    // **The positive half**, so the assertions above cannot be satisfied by marion never having
    // looked: the objects really were written, just somewhere marion owns.
    assert!(
        files_under(&agent.join("objects")) > 0,
        "a run that wrote no objects anywhere measured nothing"
    );
}

/// A directory that is not a git worktree is a **refusal that names the directory**, never a silent
/// skip — the caller turns this into `RootObservation::Failed`, and a record that says why is the
/// whole point of that variant existing.
#[test]
fn a_directory_that_is_not_a_worktree_is_refused_by_name() {
    let dir = scratch("root-change-nogit");
    let plain = dir.join("plain");
    let agent = dir.join("agent");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::create_dir_all(&agent).unwrap();
    let e = TreeSnapshot::open(&plain, &agent)
        .expect_err("a directory with no repository cannot be snapshotted");
    let msg = e.to_string();
    assert!(
        msg.contains("git") && !msg.is_empty(),
        "the refusal must be git's own words or marion's, not an empty failure: {msg}"
    );
}

/// **A state directory inside the repository under measurement is refused, by name.**
///
/// `marion run --repo /r --state-dir /r/.marion-state` puts the agent dir — and therefore the
/// copied index, the snapshot object store, the journal, the configuration and the sidecar —
/// *inside the tree `git add -A .` is about to walk. Three things go wrong at once and none of
/// them announces itself: marion's own files are measured as root activity, the object store the
/// snapshot is writing into changes underneath it while it is being written, and the record that
/// comes out is a record of marion rather than of the root.
///
/// Refusing is this repository's stated direction for a configuration it cannot honour, and the
/// refusal names **both** directories because the fix is to move one of them and an operator has
/// to know which two are in conflict. Excluding the state dir from the snapshot instead was
/// considered and rejected: the delta's claim is that it is the whole tree, and an exclusion would
/// weaken that claim for every run to rescue one misconfiguration.
#[test]
fn a_state_dir_inside_the_measured_repository_is_refused_naming_both_directories() {
    let dir = scratch("root-change-state-inside");
    let repo = fixture_repo(&dir);
    let agent = repo.join(".marion-state/project/agents/a");
    std::fs::create_dir_all(&agent).unwrap();
    let e = TreeSnapshot::open(&repo, &agent)
        .expect_err("marion must not measure a tree it is writing its own state into");
    let msg = e.to_string();
    for needle in [&*repo.display().to_string(), &*agent.display().to_string()] {
        assert!(
            msg.contains(needle),
            "the refusal must name both directories — the fix is to move one of them: `{needle}` \
             is missing from: {msg}"
        );
    }
}

// --- the record, through a real `marion run` ----------------------------------------------------

/// A `codex` stub on a directory prepended to `PATH`.
///
/// `body` is shell, run with the **root's own cwd**, which is the repository under measurement —
/// so a `> a.txt` in the body is a write by the node marion is recording, arriving through exactly
/// the route a real harness's write would.
fn stub_codex(dir: &Path, body: &str) -> PathBuf {
    stub(dir, "codex", body)
}

/// The same stub under the name the gemini adapter launches, for the grant-gate test — `gemini-impl`
/// is the built-in that declares a tool *and* runs on a `LaunchOnly` surface, so a shell script is
/// a whole harness for it.
fn stub_gemini(dir: &Path, body: &str) -> PathBuf {
    stub(dir, "gemini", body)
}

fn stub(dir: &Path, program_name: &str, body: &str) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let program = bin.join(program_name);
    std::fs::write(&program, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// One marion verb, called and answered, in codex's stream shape. Without it the run is refused as
/// `BridgeNeverReached` — which is a case this file tests deliberately and must not stumble into.
const REACHED_THE_BRIDGE: &str = r#"echo '{"type":"item.completed","item":{"id":"item_0","type":"mcp_tool_call","server":"marion","tool":"spawn","arguments":{},"status":"completed"}}'"#;

/// The same claim in **gemini's** stream shape: a `tool_use` naming the verb in gemini's own
/// `mcp_marion_*` spelling, plus the separate `tool_result` frame that says it was answered. Two
/// frames rather than one because gemini pairs them by `tool_id` — a call with no result reads as
/// `CallOutcome::Unknown`, which §6.1 step 8 refuses just as firmly as no call at all.
const REACHED_THE_BRIDGE_GEMINI: &str = concat!(
    r#"echo '{"type":"tool_use","tool_name":"mcp_marion_spawn","tool_id":"g1","parameters":{}}'"#,
    "\n",
    r#"echo '{"type":"tool_result","tool_id":"g1","status":"success","output":"ok"}'"#,
);

/// The file the gemini stub writes **only when it was actually granted an edit tool**.
///
/// gemini has no `--tools` flag: its availability axis *is* `--approval-mode auto_edit`, which is
/// what makes `write_file` and `replace` exist at all (§11 item 24, `gemini::AUTO_EDIT_APPROVAL_MODE`).
/// So a stub that consults its own argv and writes only under that mode is doing exactly what the
/// real harness does with the same grant, one layer down.
const WRITE_GRANT_WITNESS: &str = "granted.txt";

/// A gemini stub that honours its own permission axis.
///
/// **Not a stub that always writes.** A test whose harness writes unconditionally proves the file
/// got there, never that the grant did — and the failure being guarded against is precisely a
/// marion that hands out a declaration it has no record behind.
const GRANT_HONOURING_GEMINI: &str = concat!(
    "case \" $* \" in *auto_edit*) printf 'the grant reached the harness\\n' > ",
    "granted.txt",
    " ;; esac\n",
    r#"echo '{"type":"tool_use","tool_name":"mcp_marion_spawn","tool_id":"g1","parameters":{}}'"#,
    "\n",
    r#"echo '{"type":"tool_result","tool_id":"g1","status":"success","output":"ok"}'"#,
    "\nexit 0",
);

struct Run {
    code: Option<i32>,
    stderr: String,
    state: PathBuf,
    repo: PathBuf,
}

/// `marion run codex` with `bin` ahead of any real binary on `PATH`.
fn run_marion(dir: &Path, repo: &Path, state: &Path, bin: &Path, timeout_secs: &str) -> Run {
    run_marion_as(dir, repo, state, bin, timeout_secs, "codex", &[])
}

/// [`run_marion`], with the agent type and any extra flags spelled out.
///
/// The two tests below that drive §9's **grant gate** need a type that declares a tool, since the
/// gate is co-extensive with the grant and `codex` declares none. `gemini-impl` is the one used,
/// and it needs no stub on `PATH`: the gate is evaluated inside `root::prepare`, before any harness
/// process exists.
fn run_marion_as(
    dir: &Path,
    repo: &Path,
    state: &Path,
    bin: &Path,
    timeout_secs: &str,
    agent_type: &str,
    extra: &[&str],
) -> Run {
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_marion"));
    cmd.args([
        "run",
        agent_type,
        "--prompt",
        "Delegate the task to a child.",
        "--repo",
        &repo.to_string_lossy(),
        "--state-dir",
        &state.to_string_lossy(),
        // Nothing listens there. The stub is the whole model side of the run, and `--canned` is
        // what makes that legible to the binary.
        "--canned",
        "--base-url",
        "http://127.0.0.1:9/v1",
        "--timeout",
        timeout_secs,
    ]);
    cmd.args(extra);
    cmd.env("PATH", path).current_dir(dir);
    let out = run_bounded(&mut cmd, RUN_BOUND).expect("marion run starts");
    assert!(
        !out.timed_out,
        "marion run did not finish inside {RUN_BOUND:?} — the bound under test is marion's own"
    );
    Run {
        code: out.code,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        state: state.to_path_buf(),
        repo: repo.to_path_buf(),
    }
}

/// `marion run codex` in a **real one-commit repository**.
///
/// A git fixture and not a bare directory, because that is the only configuration in which there is
/// a delta to record at all — a test that ran in an unversioned directory would assert against
/// `Failed` and could never tell a working measurement from a missing one.
fn marion_run(dir: &Path, body: &str, timeout_secs: &str) -> Run {
    let repo = fixture_repo(dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let bin = stub_codex(dir, body);
    run_marion(dir, &repo, &state, &bin, timeout_secs)
}

/// [`marion_run`], in a repository whose `.gitignore` names `.env` — the blind spot §11 item 26
/// describes, set up so a test can drive a root straight into it.
///
/// The ignore rule is **committed**, not merely written, because `git add -A .` consults the
/// working tree's rules either way and a test whose fixture differed from a real repository's in
/// that respect would be measuring something else.
fn marion_run_ignoring_env(dir: &Path, body: &str) -> Run {
    let repo = fixture_repo(dir);
    std::fs::write(repo.join(".gitignore"), ".env\n").unwrap();
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
            "ignore .env",
        ],
    );
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let bin = stub_codex(dir, body);
    run_marion(dir, &repo, &state, &bin, "30")
}

/// Every file named `leaf` anywhere under `root`.
///
/// A **walk**, not a probe at a computed path, for `persisted_contracts`' reason: asserting that a
/// record is at a guessed location passes if marion wrote it somewhere else entirely, and the claim
/// these tests make is about what marion wrote, not about where a test looked.
fn find_all(root: &Path, leaf: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            find_all(&p, leaf, out);
        } else if p.file_name().is_some_and(|n| n == leaf) {
            out.push(p);
        }
    }
}

/// The run's change record, both halves, read back off disk.
///
/// Returned as a pair because the pair is the design: the journal's counts are worth something only
/// if the sidecar behind them exists, and a test that read one half could not tell a complete
/// record from a dangling pointer.
fn change_record(run: &Run) -> (RootChanged, RootChange) {
    let mut journals = Vec::new();
    find_all(&run.state, "journal.jsonl", &mut journals);
    assert_eq!(journals.len(), 1, "one project, one journal: {journals:?}");
    let records: Vec<RootChanged> = std::fs::read(&journals[0])
        .unwrap()
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(decode)
        .filter_map(|r| match r.kind {
            RecordKind::RootChanged(c) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(
        records.len(),
        1,
        "exactly one change record per run — none means the record was lost on this exit path, \
         which is the whole of NC-6"
    );

    let mut sidecars = Vec::new();
    find_all(&run.state, "root-change.json", &mut sidecars);
    assert_eq!(
        sidecars.len(),
        1,
        "the journal's counts are authoritative only because this file exists: {sidecars:?}"
    );
    let sidecar: RootChange =
        serde_json::from_slice(&std::fs::read(&sidecars[0]).unwrap()).expect("the sidecar parses");
    assert_eq!(
        sidecar.record(),
        records[0],
        "the two halves must be one reading — the journal's half is derived from the file's, so a \
         difference here means they were assembled twice"
    );
    (records[0].clone(), sidecar)
}

/// What was observed, or a panic naming what was there instead — every assertion below is about a
/// *measurement*, so a record that measured nothing must fail loudly rather than compare equal to
/// zero.
fn observed(sidecar: &RootChange) -> (&Vec<PathBuf>, &str, usize) {
    match &sidecar.working_tree_delta {
        RootDelta::Observed {
            changed_paths,
            diff,
            ..
        } => (
            changed_paths,
            diff.as_ref().map(|d| d.value.as_str()).unwrap_or(""),
            diff.as_ref().map(|d| d.original_bytes).unwrap_or(0),
        ),
        other => panic!("marion did not measure this run: {other:?}"),
    }
}

/// **NC-1 — the paired control, and the primary guard.**
///
/// Two runs of the same harness against the same fixture, differing only in whether the root wrote
/// a file. **Case A is what every silent-stop mode fails**: a snapshot that never ran, a diff never
/// taken, a record never journalled, a sidecar never written — each of them yields no `a.txt` and
/// no bytes. **Case B alone would pass under all of them**, which is exactly why B alone is not the
/// test, and why the two are one `#[test]` rather than two: neither can be `#[ignore]`d without the
/// other, and the closing assertion is that the two records *differ*, which no single-case test can
/// make at all.
#[test]
fn a_root_that_wrote_and_a_root_that_did_not_produce_different_records() {
    let content = "the agent wrote this line";

    let dir_a = scratch("root-change-wrote");
    let a = marion_run(
        &dir_a,
        &format!("printf '{content}\\n' > a.txt\n{REACHED_THE_BRIDGE}\nexit 0"),
        "30",
    );
    assert_eq!(a.code, Some(0), "case A: {}", a.stderr);
    assert!(a.repo.join("a.txt").is_file(), "the stub really did write");
    let (rec_a, side_a) = change_record(&a);
    let (paths, patch, bytes) = observed(&side_a);
    assert_eq!(paths, &vec![PathBuf::from("a.txt")]);
    assert!(
        patch.contains(content),
        "the patch must carry the bytes, not merely the path — `changed_paths` says a root touched \
         a file, a patch says whether it fixed the bug or deleted the file:\n{patch}"
    );
    assert!(bytes > 0);
    match rec_a.observation {
        RootObservation::Observed {
            changed_count,
            diff_bytes,
            dirty_at_launch,
            ..
        } => {
            assert_eq!(changed_count, 1);
            assert_eq!(diff_bytes, bytes);
            assert_eq!(dirty_at_launch, 0, "the fixture is committed clean");
        }
        other => panic!("case A measured nothing: {other:?}"),
    }

    let dir_b = scratch("root-change-quiet-run");
    let b = marion_run(&dir_b, &format!("{REACHED_THE_BRIDGE}\nexit 0"), "30");
    assert_eq!(b.code, Some(0), "case B: {}", b.stderr);
    let (rec_b, side_b) = change_record(&b);
    let (paths, patch, bytes) = observed(&side_b);
    assert!(paths.is_empty(), "case B wrote nothing: {paths:?}");
    assert!(patch.is_empty());
    assert_eq!(bytes, 0);
    match rec_b.observation {
        RootObservation::Observed {
            changed_count,
            diff_bytes,
            ..
        } => {
            assert_eq!(changed_count, 0);
            assert_eq!(diff_bytes, 0);
        }
        other => panic!("case B measured nothing: {other:?}"),
    }

    // **The assertion neither case can make alone.** A marion that always journalled the same
    // reading — the failure a single-case test cannot see — passes A or B and fails this.
    assert_ne!(
        rec_a.observation, rec_b.observation,
        "a root that wrote and a root that did not must not produce the same record; that \
         identity is the pre-`8a69f22` byte pattern this whole record exists to break"
    );
}

/// **NC-9 — a root that wrote only an ignored path must not read as a root that wrote nothing.**
///
/// `git add -A .` respects `.gitignore` (§11 item 26), so a root whose only write is `.env` moves
/// no tree: `pre_tree == post_tree`, `changed_paths: []`, an empty patch. That reading is
/// **byte-identical to the escaped-write signature the whole gate exists to distinguish from**, and
/// the record used to have no way to say so. The delta cannot be widened — the instrument is git
/// and git is what has the blind spot — so what the record must carry instead is the *size of the
/// blind spot*, which is the same triple-distinction principle as `None` / `NotAttempted` /
/// `Observed{0}`: an empty delta beside `ignored_not_measured: Some(0)` is exhaustive, and one
/// beside `Some(1)` is not.
///
/// Two runs, one fixture, one `#[test]`, for NC-1's reason. Case A writes the ignored path and case
/// B writes nothing; **both are `Observed` with an empty delta**, which is the hole stated rather
/// than hidden, and the closing assertion is that the two records nevertheless differ. Neither case
/// can make that assertion alone.
#[test]
fn a_root_that_wrote_only_an_ignored_path_does_not_read_as_a_root_that_wrote_nothing() {
    let dir_a = scratch("root-change-ignored-wrote");
    let a = marion_run_ignoring_env(
        &dir_a,
        &format!("printf 'SECRET=1\\n' > .env\n{REACHED_THE_BRIDGE}\nexit 0"),
    );
    assert_eq!(a.code, Some(0), "case A: {}", a.stderr);
    assert!(a.repo.join(".env").is_file(), "the stub really did write");
    let (rec_a, side_a) = change_record(&a);
    let (paths, patch, bytes) = observed(&side_a);
    assert!(
        paths.is_empty() && patch.is_empty() && bytes == 0,
        "this is the hole, asserted rather than assumed: git cannot see an ignored write, so the \
         delta is empty even though the root wrote — {paths:?}"
    );

    let dir_b = scratch("root-change-ignored-quiet");
    let b = marion_run_ignoring_env(&dir_b, &format!("{REACHED_THE_BRIDGE}\nexit 0"));
    assert_eq!(b.code, Some(0), "case B: {}", b.stderr);
    let (rec_b, side_b) = change_record(&b);
    let (paths, _, bytes) = observed(&side_b);
    assert!(paths.is_empty() && bytes == 0);

    assert_ne!(
        rec_a.observation, rec_b.observation,
        "a root that wrote an ignored file and a root that wrote nothing produced the same \
         record. The delta is a **git-visible** delta and always was; what a reader must still be \
         able to tell apart is an empty delta that is exhaustive from one with a blind spot behind \
         it, and today they are the same bytes — which is exactly the ambiguity §11 item 24 and \
         `8a69f22` are about"
    );
    match (&rec_a.observation, &rec_b.observation) {
        (
            RootObservation::Observed {
                ignored_not_measured: a,
                ..
            },
            RootObservation::Observed {
                ignored_not_measured: b,
                ..
            },
        ) => {
            assert_eq!(
                *b,
                Some(0),
                "case B's empty delta is exhaustive: there is nothing under an ignore rule for it \
                 to have missed, and `Some(0)` is how the record says so"
            );
            assert_eq!(
                *a,
                Some(1),
                "case A's empty delta is not exhaustive: one ignored entry exists that the delta \
                 never looked at"
            );
        }
        other => panic!("both cases must be measurements: {other:?}"),
    }
}

/// **NC-6 — the record survives every exit path.**
///
/// The post-snapshot sits at the **top** of `journal_the_roots_outcome`, before the match on the
/// run's result, and this is the test that says why. Three endings, one of them `Ok`:
///
/// * a **wall-clock kill** on the `LaunchOnly` path — the node wrote and then hung, and marion
///   killed its process group. A write that already happened is not undone by the kill, and this is
///   precisely the run whose record it would be worst to lose;
/// * **`BridgeNeverReached`** — the node wrote and never called a marion verb, so marion refuses
///   the *result*. The write is still on disk;
/// * a **clean exit**, so the other two cannot pass by a record that is written only for failures.
///
/// Red if the snapshot moves onto the `Ok` arm: the first two cases then find no record at all and
/// `change_record` fails on "exactly one change record per run".
#[test]
fn the_record_is_written_on_every_exit_path_and_not_only_a_clean_one() {
    // (label, stub body, does marion exit 0)
    let cases: [(&str, String, bool); 3] = [
        (
            "killed-on-marions-wall-clock",
            format!("printf 'x\\n' > a.txt\n{REACHED_THE_BRIDGE}\nsleep 120"),
            false,
        ),
        (
            "refused-the-bridge-was-never-reached",
            "printf 'x\\n' > a.txt\necho 'Here is a summary of the repository.'\nexit 0".into(),
            false,
        ),
        (
            "clean-exit",
            format!("printf 'x\\n' > a.txt\n{REACHED_THE_BRIDGE}\nexit 0"),
            true,
        ),
    ];
    for (label, body, clean) in cases {
        let dir = scratch(&format!("root-change-exit-{label}"));
        // Short only for the hang; the other two exit on their own and never reach the bound.
        let run = marion_run(&dir, &body, if clean { "30" } else { "4" });
        assert_eq!(run.code == Some(0), clean, "{label}: {}", run.stderr);
        let (_, sidecar) = change_record(&run);
        let (paths, patch, bytes) = observed(&sidecar);
        assert_eq!(
            paths,
            &vec![PathBuf::from("a.txt")],
            "{label}: the write happened before the ending, and an ending marion disliked does \
             not unwrite it"
        );
        assert!(bytes > 0 && patch.contains("+x"), "{label}: {patch}");
    }
}

/// **§9's grant gate, through the real binary, in both directions — and `--no-change-record` is
/// the way through it.**
///
/// `gemini-impl` declares `[read, write]`. In a directory marion cannot snapshot there is no record
/// to put behind that grant, so the run is **refused** rather than launched with a silently empty
/// tool axis — which would be a node that does no work, exits 0, and says nothing anywhere (§12).
/// The refusal has to be actionable, so all three of the directory, the declaration and the remedy
/// are asserted by name.
///
/// The second half is the escape hatch doing exactly what it says and no more: the same command
/// with `--no-change-record` reaches the harness, and the journal records `NotAttempted` **naming
/// the flag**. That is the third of the change record's three readings — *the journal is silent* /
/// *marion did not look, here is why* / *marion looked and nothing changed* — and it is the only
/// one no other test in this file produces.
#[test]
fn a_declared_grant_in_an_unrecordable_directory_is_refused_and_the_flag_is_the_way_through() {
    let dir = scratch("root-change-gate");
    let repo = dir.join("plain");
    let state = dir.join("state");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    // A stub for every half but the first. The first never starts a process: the gate is decided
    // in `prepare`, before anything is spawned or written.
    let bin = stub_gemini(&dir, GRANT_HONOURING_GEMINI);

    let refused = run_marion_as(&dir, &repo, &state, &bin, "30", "gemini-impl", &[]);
    assert_ne!(
        refused.code,
        Some(0),
        "a grant with no record behind it must not be issued:\n{}",
        refused.stderr
    );
    for needle in [
        &*repo.display().to_string(),
        "read, write",
        "--no-change-record",
    ] {
        assert!(
            refused.stderr.contains(needle),
            "the refusal must name the directory, the declaration and the remedy; `{needle}` is \
             missing from:\n{}",
            refused.stderr
        );
    }
    assert!(
        !refused.stderr.contains("disallowed"),
        "marion must never reach for a denylist to solve this (§3.1):\n{}",
        refused.stderr
    );

    let through = run_marion_as(
        &dir,
        &repo,
        &state,
        &bin,
        "30",
        "gemini-impl",
        &["--no-change-record"],
    );
    assert_eq!(
        through.code,
        Some(0),
        "the flag is the operator saying it in as many words, and then the run proceeds:\n{}",
        through.stderr
    );
    let (record, sidecar) = change_record(&through);
    match (&record.observation, &sidecar.working_tree_delta) {
        (RootObservation::NotAttempted { reason }, RootDelta::NotAttempted { .. }) => assert!(
            reason.as_str().contains("--no-change-record"),
            "an absence must name its cause, and this one's cause is a decision rather than a git \
             failure: {}",
            reason.as_str()
        ),
        other => panic!(
            "the flag must journal a decision not to look, never `Failed`, which would put a git \
             failure that never happened into the audit record: {other:?}"
        ),
    }
    assert!(
        !repo.join(WRITE_GRANT_WITNESS).exists(),
        "the harness was launched with an edit grant it should not have had"
    );

    // **The paired control, and the reason the halves above are not enough.** Everything up to here
    // can be satisfied by a marion that hands a declined root its full declaration: the stub is
    // never asked to write, so nothing looks at the grant at all, and only the exit codes and the
    // journalled `NotAttempted` go red. So: same fixture, same stub, one flag different, and the
    // stub does with its axis what a real harness does — writes only if it was granted the tool.
    let recordable = fixture_repo(&dir);
    let granted = run_marion_as(&dir, &recordable, &state, &bin, "30", "gemini-impl", &[]);
    assert_eq!(
        granted.code,
        Some(0),
        "a recordable repository is the configuration the grant exists for:\n{}",
        granted.stderr
    );
    assert!(
        recordable.join(WRITE_GRANT_WITNESS).is_file(),
        "the grant never reached the harness, so the negative half below asserts nothing:\n{}",
        granted.stderr
    );

    // Removed **before** the declined run, or its absence afterwards would be the granted run's
    // file still sitting there and the assertion would pass on a marion that granted both.
    std::fs::remove_file(recordable.join(WRITE_GRANT_WITNESS)).expect("the granted run wrote it");
    let declined = run_marion_as(
        &dir,
        &recordable,
        &state,
        &bin,
        "30",
        "gemini-impl",
        &["--no-change-record"],
    );
    assert_eq!(declined.code, Some(0), "{}", declined.stderr);
    assert!(
        !recordable.join(WRITE_GRANT_WITNESS).exists(),
        "a root whose change record was declined was handed the edit grant anyway — the witness is \
         the harness's own, so this is the grant reaching the model and not merely the flag:\n{}",
        declined.stderr
    );
}

/// The same refusal, reached the way an operator would reach it: `--state-dir` pointed inside
/// `--repo`, on the command line, with a type that declares a grant.
///
/// The unit test above pins the instrument; this pins that the instrument's refusal actually
/// travels — through `RootChangeBase::Unavailable`, through the grant gate, into an exit code and a
/// sentence an operator can act on. **Every other snapshot test in this file puts the state dir
/// outside the fixture, so without this one the configuration has no coverage at all**, which is
/// how it survived to be found by review.
#[test]
fn a_root_whose_state_dir_is_inside_the_repository_is_refused_on_the_command_line() {
    let dir = scratch("root-change-state-inside-run");
    let repo = fixture_repo(&dir);
    let state = repo.join(".marion-state");
    std::fs::create_dir_all(&state).unwrap();
    let bin = stub_gemini(&dir, &format!("{REACHED_THE_BRIDGE_GEMINI}\nexit 0"));
    let run = run_marion_as(&dir, &repo, &state, &bin, "30", "gemini-impl", &[]);
    assert_ne!(
        run.code,
        Some(0),
        "marion measured a tree it was writing its own journal, index and object store into, and \
         called the result a record of what the root did:\n{}",
        run.stderr
    );
    for needle in [
        &*repo.display().to_string(),
        &*state.display().to_string(),
        "--no-change-record",
    ] {
        assert!(
            run.stderr.contains(needle),
            "the refusal must name both directories and the way out; `{needle}` is missing \
             from:\n{}",
            run.stderr
        );
    }
}

/// A root in a directory that is **not** a git worktree still runs, and records that nothing was
/// measured.
///
/// `Failed` and not silence: this is the third of the three readings, and the one an operator
/// reaches by pointing `marion run` at a directory they never `git init`ed.
///
/// **It still runs, and that is the grant gate being co-extensive with the grant.** `codex` — the
/// type this file drives — declares no built-in tool, so there is nothing for
/// `root::availability_axis` to refuse. The same directory with `claude-impl` *is* refused; see
/// `a_declared_grant_in_an_unrecordable_directory_is_refused_and_the_flag_is_the_way_through`.
#[test]
fn a_root_outside_a_repository_runs_and_records_that_nothing_was_measured() {
    let dir = scratch("root-change-nonrepo");
    let repo = dir.join("plain");
    let state = dir.join("state");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let bin = stub_codex(&dir, &format!("{REACHED_THE_BRIDGE}\nexit 0"));
    let run = run_marion(&dir, &repo, &state, &bin, "30");
    assert_eq!(
        run.code,
        Some(0),
        "a directory with no repository is not a reason to refuse a root that declared no tool — \
         there is no grant to gate:\n{}",
        run.stderr
    );
    let (record, sidecar) = change_record(&run);
    match (&record.observation, &sidecar.working_tree_delta) {
        (RootObservation::Failed { reason }, RootDelta::Failed { .. }) => assert!(
            reason.as_str().contains("git"),
            "an absence must name its cause or it is only a shorter silence: {}",
            reason.as_str()
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(record.base_commit, None, "there was no HEAD to read");
}
