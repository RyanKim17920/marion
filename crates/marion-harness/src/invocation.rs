//! The one piece of vocabulary every adapter shares.
//!
//! It lived in `claude_code.rs` while there was only one adapter, which made the Codex adapter
//! import it from the Claude Code module — a dependency between two peers that have nothing to do
//! with each other. Design §5.2 puts `Invocation` on the `HarnessAdapter` contract itself, so it
//! belongs to neither.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The variable every launch's private temp dir rides on — what Bun, Node and Python all read for
/// "where do scratch files go".
pub const TMPDIR_ENV: &str = "TMPDIR";

/// An argv + env pair, ready to spawn. Nothing here reaches a shell.
///
/// `Debug` is hand-written: see the impl below.
#[derive(Clone, PartialEq, Eq)]
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
    /// own, the removals, the cwd, and `tmpdir` as [`TMPDIR_ENV`]. The one place a spawn site gets
    /// the removals from.
    ///
    /// **`tmpdir` is a parameter so that no launch can forget it.** Every harness process marion
    /// starts gets a temp dir the supervisor owns and deletes at reap, because a Bun-built harness
    /// unpacks its native libraries into `$TMPDIR` on every launch and never removes them — ~4.8 MB
    /// per opencode run, measured, which came to 7 GB in one day of test runs. It is set last,
    /// after the row's variables and a profile's removals, so neither can move it.
    ///
    /// **No inherited `MARION_` name reaches the harness** ([`inherited_marion_names`]): those are
    /// marion's own plumbing — a parent node's token, the supervisor's state dir, a test's gate —
    /// and the only ones a launch carries are the ones it set itself.
    pub fn command(&self, tmpdir: &Path) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.args)
            .envs(self.env.iter().cloned())
            .current_dir(&self.cwd);
        for key in &self.env_remove {
            cmd.env_remove(key);
        }
        for key in inherited_marion_names(std::env::vars_os().map(|(k, _)| k), &self.env) {
            cmd.env_remove(key);
        }
        cmd.env(TMPDIR_ENV, tmpdir);
        cmd
    }
}

/// Whether `name` is one of marion's own: every `MARION_` variable is marion's, never a harness's.
pub fn is_marion_name(name: &OsStr) -> bool {
    name.as_encoded_bytes().starts_with(b"MARION_")
}

/// The `MARION_` names among `inherited` that `set` does not set — what a launch removes from the
/// environment it would otherwise inherit.
pub fn inherited_marion_names(
    inherited: impl Iterator<Item = OsString>,
    set: &[(String, String)],
) -> Vec<OsString> {
    inherited
        .filter(|k| is_marion_name(k) && !set.iter().any(|(n, _)| OsStr::new(n) == k))
        .collect()
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
            env_remove,
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
            .field("env_remove", env_remove)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spawn_sites_temp_dir_wins_over_the_env_and_the_removals() {
        let inv = Invocation {
            program: "opencode".into(),
            args: vec![],
            env: vec![(TMPDIR_ENV.into(), "/var/folders/operator/T/".into())],
            cwd: "/repo".into(),
            model: None,
            session_mode: None,
            env_remove: vec![TMPDIR_ENV.into()],
        };
        let cmd = inv.command(Path::new("/state/agents/a-1/tmp"));
        let set: Vec<_> = cmd
            .get_envs()
            .filter(|(k, _)| *k == TMPDIR_ENV)
            .map(|(_, v)| v)
            .collect();
        assert_eq!(
            set,
            vec![Some(std::ffi::OsStr::new("/state/agents/a-1/tmp"))],
            "set last, so neither a variable nor a profile's removal can move it"
        );
    }

    /// **An inherited `MARION_` name never reaches a harness unless the launch set it itself**: a
    /// parent node's token or the supervisor's state dir is removed, the launch's own bridge
    /// variables stay, and nothing else is touched.
    #[test]
    fn inherited_marion_names_are_removed_unless_the_launch_sets_them() {
        let inherited = [
            "MARION_NODE_TOKEN",
            "MARION_STATE_DIR",
            "MARION_AGENT_ID",
            "PATH",
            "MARIONETTE",
        ]
        .map(OsString::from);
        let set = vec![("MARION_AGENT_ID".to_string(), "child".to_string())];
        assert_eq!(
            inherited_marion_names(inherited.into_iter(), &set),
            ["MARION_NODE_TOKEN", "MARION_STATE_DIR"].map(OsString::from)
        );
    }

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
            env_remove: vec![],
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
