//! **The one way marion runs git on a tree**, hardened against configuration a child can plant.
//!
//! A child runs in a worktree marion made, and it can write anything in that directory — including
//! the `.git` gitfile that tells git where the repository is. A child that replaces the gitfile
//! with a directory of its own (or points it at one) chooses the `config` every later marion git
//! call in that tree reads: `core.fsmonitor`, a hook path, a filter. marion runs `diff`, `add`,
//! `commit` and `worktree add` there as the operator, outside any sandbox the child had, so a
//! planted `core.fsmonitor` would be a program the child picked, run by marion.
//!
//! So every call goes through [`command`], which:
//!
//! * for a [`Tree::Child`], checks that `.git` is still a regular file naming a worktree registered
//!   under the operator's repository (`<common-dir>/worktrees/<name>`, whose own `gitdir` names this
//!   tree back), and refuses by name otherwise; then pins `--git-dir` and `--work-tree`, so nothing
//!   in the tree decides where git reads from, and disables hooks (`core.hooksPath=/dev/null`) as
//!   well as fsmonitor;
//! * for a [`Tree::Operator`] (the operator's own checkout: a root's tree, a `shared-cwd` child's
//!   directory, the repository a worktree is added to) disables fsmonitor only. **Hooks stay the
//!   operator's**: `git worktree add` runs their `post-checkout` hook exactly as their own `git`
//!   would, because that hook is theirs, set up in their repository, and a worktree without it can
//!   differ from one they made by hand. fsmonitor is disabled even here because it runs on reads
//!   marion makes on the operator's behalf, and nothing marion reads needs it;
//! * always sets `GIT_TERMINAL_PROMPT=0`, so no call can stop and ask for a password, and clears
//!   the variables that would redirect git elsewhere (`GIT_DIR`, `GIT_WORK_TREE`,
//!   `GIT_COMMON_DIR`).
//!
//! The operator's identity and global configuration are kept: `GIT_CONFIG_GLOBAL` and
//! `GIT_CONFIG_NOSYSTEM` are not set, so commits carry their name and their own settings apply.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::spawn::SpawnError;

/// Where a marion git call runs, and how far its configuration is trusted.
#[derive(Debug, Clone, Copy)]
pub enum Tree<'a> {
    /// A worktree marion made for a child. `repo` is the operator's repository it was added to
    /// (any worktree of it, or its main checkout): the anchor the gitfile is checked against.
    Child { wt: &'a Path, repo: &'a Path },
    /// The operator's own checkout.
    Operator(&'a Path),
}

impl<'a> Tree<'a> {
    /// The directory the call runs in.
    pub fn dir(&self) -> &'a Path {
        match self {
            Tree::Child { wt, .. } => wt,
            Tree::Operator(dir) => dir,
        }
    }
}

/// The variables that would point git at another repository, index or tree than the one named.
const REDIRECTING: [&str; 3] = ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"];

/// A `git` command for `args` on `tree`, hardened as the module doc says, with stdin closed and
/// both outputs piped. A child tree whose `.git` is not the gitfile git wrote is refused here,
/// before any git process runs in it.
pub fn command(tree: &Tree<'_>, args: &[&str]) -> Result<Command, SpawnError> {
    let mut cmd = Command::new("git");
    for key in REDIRECTING {
        cmd.env_remove(key);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(tree.dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    match tree {
        Tree::Child { wt, repo } => {
            let gitdir = verified_gitdir(wt, repo)?;
            cmd.arg(format!("--git-dir={}", gitdir.display()))
                .arg(format!("--work-tree={}", wt.display()))
                .args([
                    "-c",
                    "core.fsmonitor=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ]);
        }
        Tree::Operator(_) => {
            cmd.args(["-c", "core.fsmonitor=false"]);
        }
    }
    cmd.args(args);
    Ok(cmd)
}

/// The refusal for a child tree whose `.git` marion no longer recognises.
fn tampered(wt: &Path, why: impl std::fmt::Display) -> SpawnError {
    SpawnError::Git(
        "worktree check",
        format!(
            "{}/.git is not the gitfile git wrote for this worktree ({why}); marion runs no git \
             command in it, because its configuration would then be the child's",
            wt.display()
        ),
    )
}

/// **The git directory of child worktree `wt`, proven to be the one registered for it.**
///
/// `.git` must be a regular file (not a directory, not a symlink) reading `gitdir: <path>`; that
/// path must sit directly under `<common-dir>/worktrees/` of the operator's repository `repo`; and
/// its `gitdir` file — git's back-link, outside the child's tree — must name this tree's `.git`. A
/// child can rewrite its gitfile, but not the operator's `worktrees/` directory, so it can point
/// the gitfile neither at a repository of its own nor at a sibling's registration.
pub fn verified_gitdir(wt: &Path, repo: &Path) -> Result<PathBuf, SpawnError> {
    let dot_git = wt.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git).map_err(|e| tampered(wt, e))?;
    if !meta.file_type().is_file() {
        return Err(tampered(wt, "it is not a regular file"));
    }
    let text = std::fs::read_to_string(&dot_git).map_err(|e| tampered(wt, e))?;
    let named = text
        .strip_prefix("gitdir: ")
        .map(str::trim_end)
        .filter(|p| !p.is_empty() && !p.contains('\n'))
        .ok_or_else(|| tampered(wt, "it does not read `gitdir: <path>`"))?;
    let named = Path::new(named);
    let named = if named.is_absolute() {
        named.to_path_buf()
    } else {
        wt.join(named)
    };
    let gitdir = named.canonicalize().map_err(|e| tampered(wt, e))?;
    let registry = common_dir(repo)?.join("worktrees");
    let registry = registry.canonicalize().map_err(|e| tampered(wt, e))?;
    if gitdir.parent() != Some(registry.as_path()) {
        return Err(tampered(
            wt,
            format!(
                "it names {}, not a worktree of this repository",
                gitdir.display()
            ),
        ));
    }
    let back = std::fs::read_to_string(gitdir.join("gitdir")).map_err(|e| tampered(wt, e))?;
    let back = Path::new(back.trim_end())
        .canonicalize()
        .map_err(|e| tampered(wt, e))?;
    let ours = dot_git.canonicalize().map_err(|e| tampered(wt, e))?;
    if back != ours {
        return Err(tampered(wt, "its registration names another tree"));
    }
    Ok(gitdir)
}

/// `repo`'s common git directory, absolute, asked of git on the operator's own checkout.
fn common_dir(repo: &Path) -> Result<PathBuf, SpawnError> {
    let mut cmd = command(&Tree::Operator(repo), &["rev-parse", "--git-common-dir"])?;
    let out = crate::spawn_receive_gate::SPAWN_RECEIVE_GATE
        .spawn(&mut cmd)?
        .wait_with_output()?;
    if !out.status.success() {
        return Err(SpawnError::Git(
            "rev-parse --git-common-dir",
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    Ok(if p.is_absolute() { p } else { repo.join(p) })
}
