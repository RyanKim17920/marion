//! **No git marion or its test fixtures run inherits a redirect to another repository.**
//!
//! A test suite run from a git hook inherits the hook's `GIT_DIR` and `GIT_INDEX_FILE`, which name
//! the developer's own worktree. A fixture's `git init` once reinitialised that repository as bare
//! (`core.bare=true` in its shared config) and its `git add` wrote that worktree's index. This
//! target sets those variables, and the config injectors, to a sentinel repository for its whole
//! process and then runs what the suite runs — the fixture repository, a child worktree cut from
//! it, a child commit — and checks the sentinel is byte for byte as it was.
//!
//! Its own target, with one test, because it sets the process's environment: no other test may
//! run beside it.

use std::path::Path;

use marion_supervisor::spawn::{Tree, commit_child_work, make_worktree};
use marion_testsupport::{fixture_repo, git, scratch};

/// The sentinel's files a redirected git would write.
fn sentinel_bytes(common: &Path, admin: &Path) -> Vec<(String, Option<Vec<u8>>)> {
    [
        common.join("config"),
        common.join("index"),
        common.join("HEAD"),
        admin.join("index"),
        admin.join("HEAD"),
    ]
    .into_iter()
    .map(|p| (p.display().to_string(), std::fs::read(&p).ok()))
    .collect()
}

#[test]
fn a_redirecting_git_environment_reaches_no_other_repository() {
    let outer = scratch("git-env-sentinel");
    let sentinel = fixture_repo(&outer);
    let sentinel_wt = outer.join("sentinel-wt");
    git(
        &sentinel,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            &sentinel_wt.to_string_lossy(),
        ],
    );
    let common = sentinel.join(".git");
    let admin = common.join("worktrees").join("sentinel-wt");
    assert!(
        admin.is_dir(),
        "the sentinel's linked worktree is registered"
    );
    let before = sentinel_bytes(&common, &admin);

    // What a pre-commit hook in the sentinel's linked worktree exports, and the injectors.
    // SAFETY: this target's only test, set before any thread of its own is started.
    unsafe {
        std::env::set_var("GIT_DIR", &admin);
        std::env::set_var("GIT_INDEX_FILE", admin.join("index"));
        std::env::set_var("GIT_COMMON_DIR", &common);
        std::env::set_var("GIT_WORK_TREE", &sentinel_wt);
        std::env::set_var("GIT_CONFIG_PARAMETERS", "'core.bare'='true'");
        std::env::set_var("GIT_CONFIG_COUNT", "1");
        std::env::set_var("GIT_CONFIG_KEY_0", "core.bare");
        std::env::set_var("GIT_CONFIG_VALUE_0", "true");
    }

    let dir = scratch("git-env-fixture");
    let repo = fixture_repo(&dir);
    assert_eq!(
        git(&repo, &["rev-parse", "--absolute-git-dir"]).trim(),
        repo.join(".git").canonicalize().unwrap().to_string_lossy(),
        "the fixture is its own repository"
    );
    let wt = dir.join("child-wt");
    let (base, _) = make_worktree(
        Tree::Operator(&repo),
        &wt,
        &["marion/child".to_string()],
        None,
    )
    .expect("a child worktree is cut from the fixture");
    std::fs::write(wt.join("src/new.txt"), "new\n").unwrap();
    let tip = commit_child_work(
        Tree::Child {
            wt: &wt,
            repo: &repo,
        },
        "marion/child",
        &base,
        "child work",
    )
    .expect("the child's work is committed")
    .expect("there was work to commit");
    assert_eq!(
        git(&repo, &["rev-parse", "marion/child"]).trim(),
        tip.0,
        "on the fixture's own branch"
    );

    assert_eq!(
        sentinel_bytes(&common, &admin),
        before,
        "nothing reached the repository the environment named"
    );
}
