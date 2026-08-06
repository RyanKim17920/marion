//! **The root change record's instrument** (design §9, `marion_core::root_change`).
//!
//! A root writes in the operator's own repository, so the two things that decide whether its change
//! record is worth anything are *what the delta is measured against* and *what measuring costs the
//! operator*. Both are properties of `spawn::TreeSnapshot` and neither needs a process, so they are
//! asserted here against a real fixture repository rather than inferred from a run.
//!
//! Every test in this file is a **negative control** in the sense the design uses: it is built so
//! that it cannot be satisfied by failing to look. NC-2 goes red the day someone "simplifies" the
//! base point to `HEAD`; NC-3 goes red the moment `GIT_OBJECT_DIRECTORY` is dropped. Both were
//! watched red before they were watched green — see each test's own note.

use std::path::Path;

use marion_supervisor::spawn::TreeSnapshot;
use marion_testsupport::{fixture_repo, git, scratch};

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
/// skip — the caller turns this into `RootObservation::NotAttempted`, and a record that says why is
/// the whole point of that variant existing.
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
