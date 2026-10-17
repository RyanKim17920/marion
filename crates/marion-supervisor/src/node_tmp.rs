//! **A node's own `TMPDIR`**, made before its first process and removed once its last one is
//! reaped.
//!
//! Measured on opencode 1.18.32: every `opencode run` unpacks Bun's embedded `libfff_c.dylib`
//! (4.8 MB) into `$TMPDIR` as `.<16 hex>-00000000.dylib`, every TUI launch adds `libopentui.dylib`
//! (3.8 MB), and nothing ever deletes either. A node pointed at the operator's temp dir therefore
//! leaks once per process — 1,594 files, 7 GB, in one day of test runs. copilot leaves a
//! `node-compile-cache` there and pi a `jiti` cache, smaller and just as permanent.
//!
//! So each launched node runs with `TMPDIR` at [`AgentDir::tmp_dir`], and the guard below deletes
//! it. The guard is held by whatever holds the node's process — the synchronous launch on the
//! child and root paths — across every generation of that launch
//! (a resumed turn, a profile or credential failover), so a harness that finds its own temp files
//! again on its next process still finds them while the node lives.
//!
//! Not isolation: `TMPDIR` carries no credential for any of the seven harnesses this was measured
//! on (claude, codex, gemini, copilot, opencode, pi, goose — each one's auth status reads the same
//! under a private `TMPDIR`), so the operator's login is inherited exactly as before.
//!
//! [`AgentDir::tmp_dir`]: marion_core::paths::AgentDir::tmp_dir

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use marion_core::paths::AgentDir;

/// The node's temp dir, removed on drop — so on every exit from the launch that holds it, a panic
/// included.
#[derive(Debug)]
pub(crate) struct NodeTmp(PathBuf);

impl NodeTmp {
    /// `<agent-dir>/tmp`, 0700, created if absent. An existing one — a supervisor that died before
    /// its guard dropped — is adopted rather than refused: it is this node's and nobody else's,
    /// and this guard removes it with everything else.
    ///
    /// Made through [`crate::agent_fs::AgentFs`]: a link the node left in place of `tmp/` or of
    /// its agent dir refuses the launch, rather than pointing the node's next process at whatever
    /// it names.
    pub(crate) fn create(agent_dir: &AgentDir) -> std::io::Result<Self> {
        let path = agent_dir.tmp_dir();
        crate::agent_fs::AgentFs::open(agent_dir.path())?.dir(&path)?;
        Ok(Self(path))
    }

    /// **A fresh private temp dir at `path`**, for a launch that is not a node (`marion doctor`'s
    /// probes), whose parent is the system temp dir — `/tmp` on Linux, where anyone can make a
    /// name first. So it is one non-recursive `mkdir`, `0700`, and a name that already exists is
    /// refused (`AlreadyExists`) rather than adopted: what holds it may be someone else's.
    pub(crate) fn fresh_at(path: PathBuf) -> std::io::Result<Self> {
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for NodeTmp {
    fn drop(&mut self) {
        // Ignored: nothing can act on a failure here, and a failed cleanup must not turn a node's
        // outcome into a different one. A descendant that outlived the group kill and is still
        // writing is the one case that leaves something; the next launch of the node adopts it.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use marion_core::contract::AgentId;
    use marion_core::paths::ProjectDir;

    use super::NodeTmp;

    /// A probe's temp dir lives where anyone may name things first, so a name already held —
    /// a directory or a link — is refused, and a fresh one is made `0700`.
    #[test]
    fn a_fresh_temp_dir_refuses_a_name_already_taken() {
        let dir = marion_testsupport::scratch("node-tmp-fresh");
        let fresh = NodeTmp::fresh_at(dir.join("probe-1")).unwrap();
        let mode = std::fs::metadata(fresh.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        let taken = dir.join("probe-2");
        std::fs::create_dir(&taken).unwrap();
        let e = NodeTmp::fresh_at(taken).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        let linked = dir.join("probe-3");
        std::os::unix::fs::symlink(fresh.path(), &linked).unwrap();
        assert!(NodeTmp::fresh_at(linked).is_err());
    }

    #[test]
    fn a_nodes_temp_dir_is_private_and_goes_with_its_contents_on_drop() {
        let dir = marion_testsupport::scratch("node-tmp");
        let agent = ProjectDir::from_hash(&dir, "0123456789ab").agent(&AgentId("a-1".into()));
        let tmp = NodeTmp::create(&agent).unwrap();
        assert_eq!(tmp.path(), agent.tmp_dir());
        let mode = std::fs::metadata(tmp.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "a node's scratch files are its own");
        std::fs::create_dir(tmp.path().join("opencode")).unwrap();
        std::fs::write(
            tmp.path().join(".b9dd73e7ffbef3a2-00000000.dylib"),
            b"unpacked",
        )
        .unwrap();

        drop(tmp);

        assert!(
            !agent.tmp_dir().exists(),
            "what the harness unpacked goes with the dir"
        );
        assert!(agent.path().is_dir(), "the rest of the agent dir stays");
    }

    #[test]
    fn a_leftover_temp_dir_is_adopted_and_removed() {
        let dir = marion_testsupport::scratch("node-tmp-adopt");
        let agent = ProjectDir::from_hash(&dir, "0123456789ab").agent(&AgentId("a-1".into()));
        std::fs::create_dir_all(agent.tmp_dir()).unwrap();
        std::fs::write(agent.tmp_dir().join("left-by-a-dead-supervisor"), b"").unwrap();

        drop(NodeTmp::create(&agent).unwrap());

        assert!(!agent.tmp_dir().exists());
    }

    /// **A link the node left in place of its `tmp/`, or of its agent dir, refuses the launch**,
    /// and nothing is made, written or removed where it points.
    #[test]
    fn a_link_in_place_of_tmp_or_the_agent_dir_is_refused() {
        let dir = marion_testsupport::scratch("node-tmp-link");
        let victim = dir.join("operator");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("theirs"), b"kept").unwrap();
        let untouched = || {
            let names: Vec<_> = std::fs::read_dir(&victim)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names, ["theirs"]);
        };
        let agent = ProjectDir::from_hash(&dir, "0123456789ab").agent(&AgentId("a-1".into()));
        std::fs::create_dir_all(agent.path()).unwrap();
        std::os::unix::fs::symlink(&victim, agent.tmp_dir()).unwrap();
        assert!(NodeTmp::create(&agent).is_err());
        untouched();

        std::fs::remove_file(agent.tmp_dir()).unwrap();
        std::fs::rename(agent.path(), dir.join("moved")).unwrap();
        std::os::unix::fs::symlink(&victim, agent.path()).unwrap();
        assert!(NodeTmp::create(&agent).is_err());
        assert!(
            crate::events::EventWriter::open(&agent, &AgentId("a-1".into())).is_err(),
            "nor is the node's event stream opened through it"
        );
        untouched();
    }
}
