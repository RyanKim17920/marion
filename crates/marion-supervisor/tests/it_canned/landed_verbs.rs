//! **`marion diff`, `merge` and `shell <id>`: the home screen's `d`, `m` and `o` on the command
//! line.** Each reads the agent's detail — here from the journal of a finished project, with no
//! supervisor — and runs what the screen runs: the branch's raw log, the hardened merge, a shell
//! where it worked. The repository is a real git checkout with the agent's branch in it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use marion_testsupport::{FinishedProject, Scratch, finished_project, scratch};

/// `git` in `repo`, as a test's own commits need it: no signing, a fixed author.
fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A checkout with one commit on `main` and the child's branch one commit ahead, and the finished
/// project keyed on it.
fn bed(tag: &str) -> (Scratch, PathBuf, PathBuf, FinishedProject) {
    let dir = scratch(tag);
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "start"]);
    let state = dir.join("state");
    let project = marion_core::paths::ProjectDir::new(
        &state,
        &marion_supervisor::socket::project_root(&repo),
    );
    let fx = finished_project(&project);
    git(&repo, &["checkout", "-q", "-b", &fx.branch]);
    std::fs::write(repo.join("b.txt"), "landed by the agent\n").unwrap();
    git(&repo, &["add", "b.txt"]);
    git(&repo, &["commit", "-q", "-m", "add b"]);
    git(&repo, &["checkout", "-q", "main"]);
    (dir, repo, state, fx)
}

fn marion(args: &[&str], repo: &Path, state: &Path) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_marion"));
    cmd.args(args)
        .arg("--repo")
        .arg(repo)
        .arg("--state-dir")
        .arg(state)
        .stdin(std::process::Stdio::null());
    cmd.env_remove("MARION_STATE_DIR");
    cmd.output().expect("the marion binary runs")
}

#[test]
fn diff_shows_the_branch_and_merge_lands_it() {
    let (_dir, repo, state, fx) = bed("landed-verbs-merge");
    let diff = marion(&["diff", &fx.child.0], &repo, &state);
    let stdout = String::from_utf8_lossy(&diff.stdout);
    assert!(diff.status.success(), "{diff:?}");
    assert!(
        stdout.contains("add b") && stdout.contains("+landed by the agent"),
        "the branch's commit and its patch: {stdout}"
    );
    assert!(!repo.join("b.txt").exists(), "diff changes nothing");

    let merge = marion(&["merge", &fx.child.0], &repo, &state);
    assert!(merge.status.success(), "{merge:?}");
    assert!(
        String::from_utf8_lossy(&merge.stdout).contains(&format!("merged {}", fx.branch)),
        "{merge:?}"
    );
    assert!(repo.join("b.txt").exists(), "the branch is in the checkout");
    assert!(
        git(&repo, &["log", "-1", "--format=%P"])
            .split_whitespace()
            .count()
            == 2,
        "a merge commit, --no-ff"
    );
}

/// The root landed no branch, and says so rather than running git on nothing.
#[test]
fn a_node_with_no_branch_is_refused_with_why() {
    let (_dir, repo, state, fx) = bed("landed-verbs-none");
    for verb in ["diff", "merge"] {
        let out = marion(&[verb, &fx.root.0], &repo, &state);
        assert!(!out.status.success(), "{verb}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("landed no branch"),
            "{verb}: {out:?}"
        );
    }
}

/// `shell` runs `$SHELL` in the agent's workspace; here the shell is a script that prints where it
/// is.
#[test]
fn shell_opens_where_the_agent_worked() {
    let (dir, repo, state, fx) = bed("landed-verbs-shell");
    let project = marion_core::paths::ProjectDir::new(
        &state,
        &marion_supervisor::socket::project_root(&repo),
    );
    let worktree = project.agent(&fx.child).worktree();
    std::fs::create_dir_all(&worktree).unwrap();
    let shell = dir.join("pwd-shell");
    std::fs::write(&shell, "#!/bin/sh\npwd -P\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_marion"));
    let out = cmd
        .args(["shell", &fx.child.0, "--repo"])
        .arg(&repo)
        .arg("--state-dir")
        .arg(&state)
        .env("SHELL", &shell)
        .env_remove("MARION_STATE_DIR")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        worktree.canonicalize().unwrap().to_string_lossy()
    );
}
