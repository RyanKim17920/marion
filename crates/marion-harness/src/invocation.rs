//! The one piece of vocabulary every adapter shares.
//!
//! It lived in `claude_code.rs` while there was only one adapter, which made the Codex adapter
//! import it from the Claude Code module — a dependency between two peers that have nothing to do
//! with each other. Design §5.2 puts `Invocation` on the `HarnessAdapter` contract itself, so it
//! belongs to neither.

use std::path::PathBuf;

/// An argv + env pair, ready to spawn. Nothing here reaches a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    /// The model this invocation actually carries, in the harness's own spelling — or `None` where
    /// the harness takes none at all.
    ///
    /// It lives here, on the compiled result, rather than being re-derived by a second adapter
    /// method, for the reason every other derived path in this crate is derived once: two
    /// derivations of "which model" could disagree, and the one that ends up in
    /// `TaskContract.child.model` must be the one that went on the wire. `codex exec` takes no
    /// model argument, so the Codex adapter compiles `None` however loudly a caller asked for one.
    pub model: Option<String>,
}
