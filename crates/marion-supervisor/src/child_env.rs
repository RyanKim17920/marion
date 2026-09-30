//! **The operator's `env_passthrough`**: the variables they choose to hand one agent type's nodes
//! past the inherit filter (`marion_harness::env_filter`), from their own user-level
//! `$XDG_CONFIG_HOME/marion/env.toml`:
//!
//! ```toml
//! [passthrough]
//! codex-impl = ["MY_PROVIDER_KEY"]   # a codex `model_providers.*.env_key`
//! "*" = ["CORP_PROXY_TOKEN"]         # every agent type
//! ```
//!
//! User-level only: it names credentials the operator's shell holds, and a repository's file
//! must never widen what reaches a node.

use std::collections::BTreeMap;
use std::path::Path;

/// The file's name under the user-level config directory.
pub const ENV_FILE: &str = "env.toml";

/// The key naming every agent type.
pub const EVERY_TYPE: &str = "*";

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct EnvFile {
    #[serde(default)]
    passthrough: BTreeMap<String, Vec<String>>,
}

/// The variables the operator passes through to nodes of `agent_type`, from their [`ENV_FILE`].
/// No config directory or no file is none; a file that does not parse, or names something that is
/// not a variable or a `PREFIX_*`, is a refusal naming it.
pub fn passthrough(agent_type: &str) -> Result<Vec<String>, String> {
    match crate::credentials::config_dir() {
        Ok(dir) => passthrough_from(&dir.join(ENV_FILE), agent_type),
        Err(_) => Ok(Vec::new()),
    }
}

/// [`passthrough`] from the file at `path`.
pub fn passthrough_from(path: &Path, agent_type: &str) -> Result<Vec<String>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let file: EnvFile = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut names = Vec::new();
    for key in [EVERY_TYPE, agent_type] {
        for name in file.passthrough.get(key).into_iter().flatten() {
            let body = name.strip_suffix('*').unwrap_or(name);
            if body.is_empty() || !body.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(format!(
                    "{}: passthrough {name:?} for {key:?} is not a variable name or a PREFIX_*",
                    path.display()
                ));
            }
            names.push(name.clone());
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The operator's file hands a type its own names plus every type's, and nothing else; no file
    /// is none; a name that is not a variable is refused by name.
    #[test]
    fn passthrough_reads_the_types_names_and_every_types() {
        let dir = marion_testsupport::scratch("child-env-passthrough");
        let path = dir.join(ENV_FILE);
        assert_eq!(passthrough_from(&path, "codex-impl"), Ok(vec![]));
        std::fs::write(
            &path,
            "[passthrough]\n\"*\" = [\"CORP_*\"]\ncodex-impl = [\"MY_KEY\"]\nclaude = [\"X\"]\n",
        )
        .unwrap();
        assert_eq!(
            passthrough_from(&path, "codex-impl").unwrap(),
            ["CORP_*", "MY_KEY"]
        );
        assert_eq!(passthrough_from(&path, "gemini").unwrap(), ["CORP_*"]);
        std::fs::write(&path, "[passthrough]\ncodex-impl = [\"$(evil)\"]\n").unwrap();
        let e = passthrough_from(&path, "codex-impl").unwrap_err();
        assert!(e.contains("$(evil)"), "{e}");
    }
}
