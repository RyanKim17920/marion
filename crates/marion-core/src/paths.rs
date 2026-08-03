//! The on-disk layout of §4.3, as pure path arithmetic.
//!
//! Nothing here touches the filesystem or the environment: `marion-core` is the no-I/O layer, so
//! the supervisor reads `$MARION_STATE_DIR`, `$XDG_STATE_HOME` and `$HOME` itself and hands the
//! values in. That keeps §8's L1 tests pure *and* makes the precedence rule testable without
//! mutating process-global environment state, which is a data race in a threaded test binary.
//!
//! The layout mirrored here is normative (§4.3):
//!
//! ```text
//! <state>/<project-hash>/
//!   supervisor.sock
//!   journal.jsonl
//!   snapshot.json
//!   agents/<agent_id>/
//!     meta.json
//!     contracts/<task_id>.json
//!     events.jsonl
//!     pty.cast
//!     hook-token
//!     config/
//!     worktree
//! ```

use std::path::{Path, PathBuf};

use crate::contract::{AgentId, TaskId};

/// Length of the `<project-hash>` component: "first 12 hex of BLAKE3" (§4.3).
pub const PROJECT_HASH_LEN: usize = 12;

/// `<project-hash>` = the first 12 hex characters of BLAKE3 over the canonical project root.
///
/// The hash input is the **bytes of the path itself**, not the directory's contents — §4.3 says
/// "BLAKE3 over the canonical project root" in the same breath as calling the result a stable
/// per-project directory name, and a content hash would change on every commit, orphaning the
/// project's whole state directory. Canonicalization is the caller's job (it is I/O); this
/// function hashes exactly what it is given.
pub fn project_hash(canonical_root: &Path) -> String {
    let bytes = canonical_root.as_os_str().as_encoded_bytes();
    let digest = blake3::hash(bytes);
    // 12 hex characters = the first 6 bytes.
    let mut s = String::with_capacity(PROJECT_HASH_LEN);
    for b in &digest.as_bytes()[..PROJECT_HASH_LEN / 2] {
        s.push(char::from_digit((b >> 4) as u32, 16).expect("nibble is < 16"));
        s.push(char::from_digit((b & 0x0f) as u32, 16).expect("nibble is < 16"));
    }
    s
}

/// `<state>` = `$MARION_STATE_DIR`, else `$XDG_STATE_HOME/marion`, else `$HOME/.local/state/marion`
/// (§4.3).
///
/// Every input is supplied, never read. An **empty** value counts as unset: a shell exports
/// `FOO=` for a variable it means to leave blank, and resolving `<state>` to a relative path
/// rooted at the process cwd would put an agent's state somewhere different on every invocation.
///
/// `None` when even `$HOME` is absent — a real condition in a bare service manager, and one the
/// supervisor must report rather than paper over with a guess.
pub fn state_dir(
    marion_state_dir: Option<&str>,
    xdg_state_home: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    fn set(v: Option<&str>) -> Option<&str> {
        v.filter(|s| !s.is_empty())
    }
    if let Some(explicit) = set(marion_state_dir) {
        return Some(PathBuf::from(explicit));
    }
    if let Some(xdg) = set(xdg_state_home) {
        return Some(Path::new(xdg).join("marion"));
    }
    set(home).map(|h| Path::new(h).join(".local").join("state").join("marion"))
}

/// `<state>/<project-hash>` and everything under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDir(PathBuf);

impl ProjectDir {
    /// Build from an already-resolved `<state>` and a canonical project root.
    pub fn new(state: &Path, canonical_root: &Path) -> Self {
        Self(state.join(project_hash(canonical_root)))
    }

    /// Build from a `<project-hash>` computed elsewhere (a journal replay names the directory it
    /// was read from; recomputing the hash would need the project root to still exist).
    pub fn from_hash(state: &Path, project_hash: &str) -> Self {
        Self(state.join(project_hash))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// §2/§4.3: the unix socket. The 104-byte `sun_path` check and the `/tmp` fallback are the
    /// supervisor's, since both depend on the platform's real limit.
    pub fn supervisor_sock(&self) -> PathBuf {
        self.0.join("supervisor.sock")
    }

    pub fn journal(&self) -> PathBuf {
        self.0.join("journal.jsonl")
    }

    pub fn snapshot(&self) -> PathBuf {
        self.0.join("snapshot.json")
    }

    pub fn agents_dir(&self) -> PathBuf {
        self.0.join("agents")
    }

    /// `<agent-dir>`. `AgentId` is used **verbatim** as the path component (§6.7), which is safe
    /// precisely because it is a lowercase hyphenated UUIDv7 — see `crate::ids`.
    pub fn agent(&self, id: &AgentId) -> AgentDir {
        AgentDir(self.agents_dir().join(&id.0))
    }
}

/// `<state>/<project-hash>/agents/<agent_id>` and its contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDir(PathBuf);

impl AgentDir {
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Compiled spec, caps, harness ref, binary path + version.
    pub fn meta(&self) -> PathBuf {
        self.0.join("meta.json")
    }

    pub fn contracts_dir(&self) -> PathBuf {
        self.0.join("contracts")
    }

    /// `contracts/<task_id>.json` — child nodes only; a root has none (§9). One per run, so a
    /// resume opens another file rather than overwriting the first (§6.7).
    pub fn contract(&self, task: &TaskId) -> PathBuf {
        self.contracts_dir().join(format!("{}.json", task.0))
    }

    /// IR, append-only.
    pub fn events(&self) -> PathBuf {
        self.0.join("events.jsonl")
    }

    /// asciicast v3; exists only for a surface with a pty.
    pub fn pty_cast(&self) -> PathBuf {
        self.0.join("pty.cast")
    }

    /// 0600, per-node Stop-hook token (§5.4). The mode is the supervisor's to set.
    pub fn hook_token(&self) -> PathBuf {
        self.0.join("hook-token")
    }

    /// Isolated harness config dir, if any (§6.4).
    pub fn config_dir(&self) -> PathBuf {
        self.0.join("config")
    }

    /// Symlink to the worktree, when `isolation: worktree`.
    pub fn worktree(&self) -> PathBuf {
        self.0.join("worktree")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn precedence_is_marion_then_xdg_then_home() {
        assert_eq!(
            state_dir(Some("/explicit"), Some("/xdg"), Some("/home/u")),
            Some(p("/explicit")),
            "MARION_STATE_DIR wins outright"
        );
        assert_eq!(
            state_dir(None, Some("/xdg"), Some("/home/u")),
            Some(p("/xdg/marion")),
            "XDG_STATE_HOME is joined with `marion`, not used bare"
        );
        assert_eq!(
            state_dir(None, None, Some("/home/u")),
            Some(p("/home/u/.local/state/marion")),
            "the HOME fallback is the full XDG-default path"
        );
        assert_eq!(
            state_dir(None, None, None),
            None,
            "no HOME is reportable, not guessable"
        );
    }

    #[test]
    fn an_empty_env_value_counts_as_unset() {
        // A shell exports `XDG_STATE_HOME=` for a variable it means to leave blank. Treating it as
        // set would resolve `<state>` to the relative path `marion`, rooted at whatever cwd the
        // supervisor happened to start in — a different state directory per invocation.
        assert_eq!(
            state_dir(Some(""), Some(""), Some("/home/u")),
            Some(p("/home/u/.local/state/marion"))
        );
    }

    #[test]
    fn project_hash_is_twelve_lowercase_hex_of_the_path_bytes() {
        let h = project_hash(Path::new("/Users/u/code/marion"));
        assert_eq!(h.len(), PROJECT_HASH_LEN);
        assert!(
            h.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "got {h}"
        );
        // Pinned against the blake3 of the path *bytes*: if the input ever silently changed to,
        // say, the directory's contents, this value would move and every existing project's state
        // directory would be orphaned.
        let full = blake3::hash(b"/Users/u/code/marion").to_hex();
        assert_eq!(h, full[..PROJECT_HASH_LEN]);
        // And that the algorithm really is BLAKE3, checked against RFC-style published vector for
        // the empty input — a swapped hash crate would silently rename every project directory.
        assert_eq!(project_hash(Path::new("")), "af1349b9f5f9");
    }

    #[test]
    fn project_hash_is_stable_and_distinguishes_roots() {
        assert_eq!(project_hash(Path::new("/a")), project_hash(Path::new("/a")));
        assert_ne!(
            project_hash(Path::new("/a/proj")),
            project_hash(Path::new("/b/proj")),
            "two projects with the same basename must not share a state directory"
        );
    }

    #[test]
    fn the_tree_matches_the_normative_layout() {
        let state = p("/s");
        let dir = ProjectDir::from_hash(&state, "0123456789ab");
        assert_eq!(dir.path(), p("/s/0123456789ab"));
        assert_eq!(dir.supervisor_sock(), p("/s/0123456789ab/supervisor.sock"));
        assert_eq!(dir.journal(), p("/s/0123456789ab/journal.jsonl"));
        assert_eq!(dir.snapshot(), p("/s/0123456789ab/snapshot.json"));
        assert_eq!(dir.agents_dir(), p("/s/0123456789ab/agents"));

        let a = dir.agent(&AgentId("0197f3aa-1c2d-7e00-8000-0102030405f0".into()));
        let base = p("/s/0123456789ab/agents/0197f3aa-1c2d-7e00-8000-0102030405f0");
        assert_eq!(a.path(), base);
        assert_eq!(a.meta(), base.join("meta.json"));
        assert_eq!(a.contracts_dir(), base.join("contracts"));
        assert_eq!(a.events(), base.join("events.jsonl"));
        assert_eq!(a.pty_cast(), base.join("pty.cast"));
        assert_eq!(a.hook_token(), base.join("hook-token"));
        assert_eq!(a.config_dir(), base.join("config"));
        assert_eq!(a.worktree(), base.join("worktree"));
        assert_eq!(
            a.contract(&TaskId("0197f3aa-1c2d-7e01-8000-0102030405f1".into())),
            base.join("contracts/0197f3aa-1c2d-7e01-8000-0102030405f1.json")
        );
    }

    #[test]
    fn new_and_from_hash_agree() {
        let root = Path::new("/Users/u/code/marion");
        assert_eq!(
            ProjectDir::new(Path::new("/s"), root),
            ProjectDir::from_hash(Path::new("/s"), &project_hash(root)),
            "replay names a directory by its hash; it must land on the same path as a fresh resolve"
        );
    }

    #[test]
    fn an_agent_id_is_one_path_component() {
        // Cap rule 6 and §6.7 both lean on `contracts/<task_id>.json` being a fixed-length leaf.
        let a = ProjectDir::from_hash(Path::new("/s"), "0123456789ab")
            .agent(&AgentId("0197f3aa-1c2d-7e00-8000-0102030405f0".into()));
        let rel = a.path().strip_prefix("/s/0123456789ab/agents").unwrap();
        assert_eq!(
            rel.components().count(),
            1,
            "a UUIDv7 must never introduce a nested path"
        );
    }
}
