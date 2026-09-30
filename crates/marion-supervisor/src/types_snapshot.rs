//! **The agent-types table a tree of nodes runs under, fixed when its root started.**
//!
//! `.marion/agents.toml` is a tracked file of the tree it sits in, and a child's worktree is a
//! copy of that tree the child can edit. Read afresh at every spawn, the file a child rewrote in
//! its own worktree decided what its descendants were: a new `acp:` row, a wider tool list, a
//! larger `max_depth` for its own type. So the table is read **once**, from the root's tree when
//! the root is started, written into the root's agent directory, and handed down: every child's
//! spawn copies its caller's snapshot into its own agent directory, and resolves types — its own,
//! its caller's and its children's — through it. The agent directories are marion's state, outside
//! every worktree.
//!
//! The root's own type is recorded too, and its `max_depth` bounds the whole tree
//! ([`TypesSnapshot::bounded`]): no descendant's type can extend delegation past what the root was
//! started with.

use std::path::{Path, PathBuf};

use marion_core::agent_type::{AgentType, AgentTypes};
use marion_core::contract::AgentId;
use marion_core::paths::ProjectDir;
use serde::{Deserialize, Serialize};

use crate::spawn::SpawnError;

/// The file each node's agent directory holds its tree's table in.
pub const SNAPSHOT_FILE: &str = "agent-types.json";

/// The table a node's tree was started under. See the module doc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypesSnapshot {
    /// Where the root's tree kept its `.marion/agents.toml`, for the repo-trust check, which is
    /// keyed on the file's path and the digest of this very text. `None` with no file.
    source: Option<PathBuf>,
    /// The file's text as read when the root started. `None` with no file: the built-ins.
    text: Option<String>,
    /// The root's agent type, whose `max_depth` bounds the tree. `None` for a table read on behalf
    /// of a caller marion holds no snapshot for (see [`for_caller`]).
    root_type: Option<String>,
    /// **The operator let sandboxed nodes in this tree spawn less-contained children**: their
    /// user-level `[delegation] allow_wider_children`, or `marion run
    /// --allow-wider-children`, read when the root started ([`crate::user_config`]). Never from the
    /// tree itself.
    #[serde(default)]
    allow_wider_children: bool,
    /// **The writable scope this node was granted** by its caller's spawn: what its own children's
    /// scopes must stay inside. `None` for a root, which is bounded by its type's ceiling alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    granted_scope: Option<Vec<marion_core::contract::Glob>>,
    /// **This node is a session the operator started in a read-only mode** (`marion claude
    /// --permission-mode plan`, `marion codex --sandbox read-only`, as its row states them): it
    /// counts as a read-only node that delegates nothing that writes, whatever its type grants.
    /// A root's alone — never handed down, since each child's authority is its own type's.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    session_read_only: bool,
}

impl TypesSnapshot {
    /// Read `tree`'s table now, for a root of type `root_type` starting in it.
    pub fn take(tree: &Path, root_type: Option<&str>) -> Result<TypesSnapshot, SpawnError> {
        let file = crate::run::read_agent_types(tree)?;
        let (source, text) = match file {
            Some((path, text)) => (Some(path), Some(text)),
            None => (None, None),
        };
        Ok(TypesSnapshot {
            source,
            text,
            root_type: root_type.map(str::to_string),
            allow_wider_children: false,
            granted_scope: None,
            session_read_only: false,
        })
    }

    /// This snapshot for a root whose session the operator started read-only (or not).
    pub fn with_read_only_session(self, read_only: bool) -> TypesSnapshot {
        TypesSnapshot {
            session_read_only: read_only,
            ..self
        }
    }

    /// Whether the node holding this snapshot is a session started in a read-only mode.
    pub fn read_only_session(&self) -> bool {
        self.session_read_only
    }

    /// This snapshot for a child granted `scope`: what that child's own children must stay inside.
    pub fn granting(self, scope: Vec<marion_core::contract::Glob>) -> TypesSnapshot {
        TypesSnapshot {
            granted_scope: Some(scope),
            session_read_only: false,
            ..self
        }
    }

    /// The writable scope the node holding this snapshot was granted, `None` for a root.
    pub fn granted_scope(&self) -> Option<&[marion_core::contract::Glob]> {
        self.granted_scope.as_deref()
    }

    /// This snapshot with the operator's containment opt-in set as stated.
    pub fn allowing_wider_children(self, allow: bool) -> TypesSnapshot {
        TypesSnapshot {
            allow_wider_children: allow,
            ..self
        }
    }

    /// Whether the operator let sandboxed nodes in this tree spawn less-contained children.
    pub fn allows_wider_children(&self) -> bool {
        self.allow_wider_children
    }

    /// The snapshot in `agent_dir`, or `None` where the node has none.
    pub fn read(agent_dir: &Path) -> Result<Option<TypesSnapshot>, SpawnError> {
        let path = agent_dir.join(SNAPSHOT_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes)
                    .map(Some)
                    .map_err(|e| SpawnError::AgentTypesFile {
                        path,
                        error: format!("marion wrote this snapshot and cannot read it back: {e}"),
                    })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SpawnError::AgentTypesFile {
                path,
                error: e.to_string(),
            }),
        }
    }

    /// Keep this snapshot in `agent_dir`, owner-only.
    pub fn write(&self, agent_dir: &Path) -> Result<(), SpawnError> {
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        crate::private_fs::write_atomic(&agent_dir.join(SNAPSHOT_FILE), &bytes)?;
        Ok(())
    }

    /// The table: the built-ins plus the snapshot's rows, checked as a spawn checks a file.
    pub fn types(&self) -> Result<AgentTypes, SpawnError> {
        match (&self.source, &self.text) {
            (Some(path), Some(text)) => crate::run::agent_types_text(path.clone(), text),
            _ => Ok(AgentTypes::builtins_only()),
        }
    }

    /// The type a launch runs: `name` resolved through the table, refused unless every command a
    /// repository row names has the operator's consent ([`crate::trust::require`], over the text
    /// the snapshot holds), and bounded by the root's `max_depth`.
    pub fn launch_type(&self, name: &str) -> Result<AgentType, SpawnError> {
        let types = self.types()?;
        let ty = types
            .resolve(name)
            .ok_or_else(|| SpawnError::UnknownAgentType(name.to_string()))?;
        if let (Some(path), Some(text)) = (&self.source, &self.text)
            && types.user().iter().any(|u| u.name == ty.name)
        {
            crate::trust::require(path, text, &ty)?;
        }
        Ok(self.bounded_with(&types, ty))
    }

    /// `name` resolved through the table and bounded, with no trust check: for describing a node
    /// that already runs (its caller's type, a reviewer's note), never for launching one.
    pub fn resolve(&self, name: &str) -> Result<Option<AgentType>, SpawnError> {
        let types = self.types()?;
        Ok(types.resolve(name).map(|ty| self.bounded_with(&types, ty)))
    }

    /// `ty` with its `max_depth` no deeper than the root type's.
    pub fn bounded(&self, ty: AgentType) -> Result<AgentType, SpawnError> {
        Ok(self.bounded_with(&self.types()?, ty))
    }

    fn bounded_with(&self, types: &AgentTypes, mut ty: AgentType) -> AgentType {
        if let Some(root) = self.root_type.as_deref().and_then(|r| types.resolve(r)) {
            ty.max_depth = ty.max_depth.min(root.max_depth);
        }
        ty
    }
}

/// **The table a caller's spawn resolves through**: the snapshot in the caller's agent directory.
///
/// A caller marion holds none for is read from `tree` only where `tree` is the operator's own
/// checkout (a root's tree, or a `shared-cwd` directory), exactly as a root's table is; a caller
/// running in a worktree marion made for a child is refused, because reading that tree is the
/// rewrite this module exists to prevent.
///
/// `caller_type` bounds the tree where no snapshot records a root type: the caller is then the
/// highest node marion can see.
pub fn for_caller(
    project: &ProjectDir,
    caller: &AgentId,
    tree: &Path,
    caller_type: &str,
) -> Result<TypesSnapshot, SpawnError> {
    if let Some(snapshot) = TypesSnapshot::read(project.agent(caller).path())? {
        return Ok(snapshot);
    }
    if tree.starts_with(project.agents_dir()) {
        return Err(SpawnError::AgentTypesFile {
            path: project.agent(caller).path().join(SNAPSHOT_FILE),
            error: format!(
                "marion holds no record of the agent types node `{}`'s tree started with, and \
                 will not read them from the worktree it runs in, which it can edit",
                caller.0
            ),
        });
    }
    let allow =
        crate::user_config::allow_wider_children().map_err(|error| SpawnError::AgentTypesFile {
            path: crate::user_config::path().unwrap_or_default(),
            error,
        })?;
    Ok(TypesSnapshot::take(tree, Some(caller_type))?.allowing_wider_children(allow))
}

/// **The table a node the operator asked for directly starts its tree with**: `tree`'s, read now,
/// with the requested type as the tree's top — as a root's table is ([`TypesSnapshot::take`]), since
/// no node's snapshot is above it. A tree under this project's agent directories is a worktree
/// marion made for a node, which that node can edit, and is refused for [`for_caller`]'s reason.
///
/// `allow_wider_children` is the operator's flag for this node; their config file can also say so.
///
/// A **resumed** node's tree keeps the table its first life started with, from its own agent
/// directory, never a fresh read: the repository may have changed its file since.
pub fn for_operator(
    project: &ProjectDir,
    resumed: Option<&AgentId>,
    tree: &Path,
    agent_type: &str,
    allow_wider_children: bool,
) -> Result<TypesSnapshot, SpawnError> {
    if let Some(node) = resumed {
        return TypesSnapshot::read(project.agent(node).path())?.ok_or_else(|| {
            SpawnError::AgentTypesFile {
                path: project.agent(node).path().join(SNAPSHOT_FILE),
                error: format!(
                    "marion holds no record of the agent types node `{}`'s tree started with, \
                     and will not read them afresh for its second life",
                    node.0
                ),
            }
        });
    }
    let allow = allow_wider_children
        || crate::user_config::allow_wider_children().map_err(|error| {
            SpawnError::AgentTypesFile {
                path: crate::user_config::path().unwrap_or_default(),
                error,
            }
        })?;
    Ok(TypesSnapshot::take(tree, Some(agent_type))?.allowing_wider_children(allow))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The root's `max_depth` bounds every type in its tree**: a type allowing more is clamped
    /// to the root type's, and one allowing less keeps its own.
    #[test]
    fn the_roots_max_depth_bounds_every_type_in_its_tree() {
        let dir = marion_testsupport::scratch("types-snapshot-depth");
        let snapshot = TypesSnapshot::take(&dir, Some("claude")).unwrap();
        let root = marion_core::agent_type::builtin("claude")
            .unwrap()
            .max_depth;
        let mut deep = marion_core::agent_type::builtin("codex").unwrap();
        deep.max_depth = root + 6;
        assert_eq!(snapshot.bounded(deep).unwrap().max_depth, root);
        let mut shallow = marion_core::agent_type::builtin("codex").unwrap();
        shallow.max_depth = 1;
        assert_eq!(snapshot.bounded(shallow).unwrap().max_depth, 1);
    }

    /// **A tree's table is not read through a symlink that leaves the tree**: the file is the
    /// tree's, and a link to a file elsewhere would let a checkout name rows it does not hold.
    #[test]
    fn a_table_linked_from_outside_the_tree_is_refused() {
        let dir = marion_testsupport::scratch("types-snapshot-symlink");
        let tree = dir.join("tree");
        std::fs::create_dir_all(tree.join(".marion")).unwrap();
        let outside = dir.join("elsewhere.toml");
        std::fs::write(
            &outside,
            "[[agent]]\nname = \"r\"\nharness = \"codex\"\ndescription = \"d\"\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, tree.join(crate::run::AGENT_TYPES_FILE)).unwrap();
        let err = crate::run::agent_types(&tree).expect_err("a link out of the tree is refused");
        assert!(err.to_string().contains("outside"), "{err}");

        // A link that stays inside the tree is the tree's own file.
        std::fs::remove_file(tree.join(crate::run::AGENT_TYPES_FILE)).unwrap();
        std::fs::copy(&outside, tree.join("types.toml")).unwrap();
        std::os::unix::fs::symlink("../types.toml", tree.join(crate::run::AGENT_TYPES_FILE))
            .unwrap();
        assert!(
            crate::run::agent_types(&tree)
                .unwrap()
                .resolve("r")
                .is_some()
        );
    }

    /// A snapshot survives its round trip through the agent directory byte for byte.
    #[test]
    fn a_snapshot_reads_back_as_written() {
        let dir = marion_testsupport::scratch("types-snapshot-roundtrip");
        let tree = dir.join("tree");
        std::fs::create_dir_all(tree.join(".marion")).unwrap();
        std::fs::write(
            tree.join(crate::run::AGENT_TYPES_FILE),
            "[[agent]]\nname = \"r\"\nharness = \"codex\"\ndescription = \"d\"\n",
        )
        .unwrap();
        let taken = TypesSnapshot::take(&tree, Some("claude")).unwrap();
        let agent_dir = dir.join("agent");
        taken.write(&agent_dir).unwrap();
        assert_eq!(TypesSnapshot::read(&agent_dir).unwrap(), Some(taken));
        assert_eq!(TypesSnapshot::read(&dir.join("none")).unwrap(), None);
    }
}
