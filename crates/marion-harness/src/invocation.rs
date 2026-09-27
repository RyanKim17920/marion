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
    /// Variables to **remove** from the inherited environment — a profile's `clear` list
    /// ([`crate::profile::ProfileCarrier::clear`]). Empty on every launch without a profile.
    pub env_remove: Vec<String>,
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
    /// The session mode an ACP launch asks its driver to set — the agent type's `approval_mode`,
    /// applied over the protocol after `session/new` and refused by name where the agent does not
    /// offer it — or `None`, the agent's own default. Always `None` off ACP, whose other harnesses
    /// have no session to set one in.
    pub session_mode: Option<String>,
}

impl Invocation {
    /// A process builder for this invocation: program, argv, the environment layered on marion's
    /// own, the removals, the cwd. The one place a spawn site gets the removals from.
    pub fn command(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.args)
            .envs(self.env.iter().cloned())
            .current_dir(&self.cwd);
        for key in &self.env_remove {
            cmd.env_remove(key);
        }
        cmd
    }
}
