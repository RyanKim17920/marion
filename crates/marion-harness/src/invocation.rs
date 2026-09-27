//! The one piece of vocabulary every adapter shares.
//!
//! It lived in `claude_code.rs` while there was only one adapter, which made the Codex adapter
//! import it from the Claude Code module — a dependency between two peers that have nothing to do
//! with each other. Design §5.2 puts `Invocation` on the `HarnessAdapter` contract itself, so it
//! belongs to neither.

use std::path::PathBuf;

/// An argv + env pair, ready to spawn. Nothing here reaches a shell.
///
/// `Debug` is hand-written: see the impl below.
#[derive(Clone, PartialEq, Eq)]
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
    /// The session mode an ACP launch asks its driver to set — the agent type's `approval_mode`,
    /// applied over the protocol after `session/new` and refused by name where the agent does not
    /// offer it — or `None`, the agent's own default. Always `None` off ACP, whose other harnesses
    /// have no session to set one in.
    pub session_mode: Option<String>,
}

/// **`env` prints its names, never its values.** The environment is the channel every credential
/// marion hands a harness travels on — the canned run token as `ANTHROPIC_AUTH_TOKEN`, an endpoint
/// key as the row's key variable, a node's capability as `MARION_NODE_TOKEN`, a whole config
/// document with a key inside it — and an `Invocation` is what a launch failure has in hand. The
/// names alone still say what was set, which is what a reader of a failure needs.
///
/// Destructured rather than read field by field, so a field added to the struct is a compile error
/// here until someone decides how it prints.
impl std::fmt::Debug for Invocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Invocation {
            program,
            args,
            env,
            cwd,
            model,
            session_mode,
        } = self;
        struct Names<'a>(&'a [(String, String)]);
        impl std::fmt::Debug for Names<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_list()
                    .entries(self.0.iter().map(|(k, _)| format!("{k}=***")))
                    .finish()
            }
        }
        f.debug_struct("Invocation")
            .field("program", program)
            .field("args", args)
            .field("env", &Names(env))
            .field("cwd", cwd)
            .field("model", model)
            .field("session_mode", session_mode)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_names_every_variable_and_prints_no_value() {
        let inv = Invocation {
            program: "claude".into(),
            args: vec!["-p".into()],
            env: vec![
                ("ANTHROPIC_AUTH_TOKEN".into(), "marion-run-SENTINEL".into()),
                ("ANTHROPIC_API_KEY".into(), String::new()),
            ],
            cwd: "/repo".into(),
            model: Some("haiku".into()),
            session_mode: None,
        };
        let printed = format!("{inv:?} {inv:#?}");
        assert!(!printed.contains("SENTINEL"), "{printed}");
        for shown in [
            "ANTHROPIC_AUTH_TOKEN=***",
            "ANTHROPIC_API_KEY=***",
            "\"-p\"",
            "haiku",
        ] {
            assert!(printed.contains(shown), "{shown} missing from {printed}");
        }
    }
}
