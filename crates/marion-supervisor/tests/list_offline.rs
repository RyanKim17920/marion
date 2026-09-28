//! **`marion list` and `marion ls <id>` answer from the journal once nobody is serving.**
//!
//! A run that is over leaves no supervisor behind — every node exited, and the supervisor with
//! them — and the operator's next question is what happened. Both verbs used to refuse then with
//! "no supervisor is serving", which answered a question nobody asked. These run the real binary
//! against a finished project written straight to disk ([`marion_testsupport::finished_project`])
//! and prove the two properties that matter: the finished nodes are printed in the live path's
//! own lines, and the look is strictly a read — no supervisor started, no lock, no socket, and not
//! one byte under the state dir changed.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use marion_testsupport::{FinishedProject, Scratch, finished_project, scratch, survivors};

/// A finished project and its repo (no git, so the project key is the directory itself).
fn bed(tag: &str) -> (Scratch, PathBuf, PathBuf, FinishedProject) {
    let dir = scratch(tag);
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let state = dir.join("state");
    let project = marion_core::paths::ProjectDir::new(
        &state,
        &marion_supervisor::socket::project_root(&repo),
    );
    let fx = finished_project(&project);
    (dir, repo, state, fx)
}

/// The binary with this bed's project flags. The state-dir variable is removed so the flag is the
/// only thing choosing the state dir, whatever the shell running the suite exports.
fn marion(args: &[&str], repo: &Path, state: &Path) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_marion"));
    cmd.args(args)
        .arg("--repo")
        .arg(repo)
        .arg("--state-dir")
        .arg(state)
        .stdin(std::process::Stdio::null());
    Command::env_remove(&mut cmd, "MARION_STATE_DIR");
    cmd.output().expect("the marion binary runs")
}

/// Every path under `root` with its bytes.
fn contents(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(dir) = todo.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.push((path.clone(), Vec::new()));
                todo.push(path);
            } else {
                out.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
    }
    out.sort();
    out
}

/// The strict-read half, asserted after each verb: nothing under the state dir moved, no lock or
/// socket appeared, and no process is running against this state dir.
fn assert_untouched(state: &Path, before: &[(PathBuf, Vec<u8>)]) {
    let after = contents(state);
    assert_eq!(after, before, "a listing changed the state dir");
    assert!(
        !after.iter().any(|(p, _)| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name == "supervisor.lock" || name.ends_with(".sock")
        }),
        "a listing left a supervisor's lock or socket: {:?}",
        after.iter().map(|(p, _)| p).collect::<Vec<_>>()
    );
    let running = survivors(&state.display().to_string());
    assert!(
        running.is_empty(),
        "a listing started a process: {running:?}"
    );
}

#[test]
fn list_prints_the_finished_nodes_from_the_journal_without_a_supervisor() {
    let (_dir, repo, state, fx) = bed("list-offline-list");
    let before = contents(&state);

    let out = marion(&["list"], &repo, &state);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(
        !stderr.contains("start a session here first"),
        "the refusal came back: {stderr}"
    );
    assert!(
        stderr.contains("journal"),
        "the note that this is the journal's record is missing: {stderr}"
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "one line per node: {stdout}");
    assert!(
        lines[0].contains(&fx.root.0) && lines[0].contains(" exited:ok "),
        "{stdout}"
    );
    assert!(
        lines[1].contains(&fx.child.0) && lines[1].contains(" parent "),
        "{stdout}"
    );
    assert_untouched(&state, &before);

    // `ls` with no node and no terminal prints the same lines, by the same path.
    let ls = marion(&["ls"], &repo, &state);
    assert!(
        ls.status.success(),
        "{}",
        String::from_utf8_lossy(&ls.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&ls.stdout), stdout);
    assert_untouched(&state, &before);
}

#[test]
fn ls_of_a_finished_node_prints_its_contract_from_the_journal_without_a_supervisor() {
    let (_dir, repo, state, fx) = bed("list-offline-ls");
    let before = contents(&state);

    let out = marion(&["ls", &fx.child.0], &repo, &state);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    let first = stdout.lines().next().unwrap_or("");
    assert!(
        first.contains(&fx.child.0) && first.contains(" exited:ok "),
        "the node's own list line heads it: {stdout}"
    );
    for (key, want) in [
        ("task", "--top N".to_string()),
        ("tokens", "1200 in · 340 out · 5000 cached".to_string()),
        ("branch", fx.branch.clone()),
        ("result", "done".to_string()),
        ("merge", format!("git merge --no-ff {}", fx.branch)),
    ] {
        assert!(
            stdout
                .lines()
                .any(|l| l.trim_start().starts_with(key) && l.contains(&want)),
            "no `{key}` line carrying `{want}`: {stdout}"
        );
    }
    assert_untouched(&state, &before);

    let unknown = marion(&["ls", "nobody"], &repo, &state);
    assert!(!unknown.status.success());
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("`nobody`"),
        "{}",
        String::from_utf8_lossy(&unknown.stderr)
    );
    assert_untouched(&state, &before);
}
