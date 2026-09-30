//! **The operator's own marion settings**: `$XDG_CONFIG_HOME/marion/config.toml` (else
//! `~/.config/marion/config.toml`).
//!
//! User-level only. A repository's `.marion/` is never read for anything here, and neither is an
//! MCP call or an agent type: what this file decides is the operator's alone to decide.
//!
//! ```toml
//! [delegation]
//! # Let a node start a child with more authority than its own (a sandboxed codex starting
//! # claude, a read-only planner starting an implementer). Each such spawn is journaled and
//! # shown on the node.
//! allow_wider_children = true
//!
//! [limits]
//! # Raise the ceilings every node starts under (see `node_limits`).
//! max_open_files = 16384
//! max_processes = 8192
//! ```

use std::path::PathBuf;

use serde::Deserialize;

/// The file's name under marion's config directory.
pub const CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    delegation: Delegation,
    #[serde(default)]
    limits: Limits,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Limits {
    max_open_files: Option<u64>,
    max_processes: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Delegation {
    #[serde(default)]
    allow_wider_children: bool,
}

/// Where the file is, or would be.
pub fn path() -> Option<PathBuf> {
    crate::credentials::config_dir()
        .ok()
        .map(|d| d.join(CONFIG_FILE))
}

/// **Whether the operator allows a node to start a child with more authority than its own**, from
/// [`path`]. No file, or no key, is `false`. A file that exists and cannot be read or parsed is an
/// error naming it: an opt-in the operator believes they set must not silently read as off, and a
/// typo must not silently read as on.
pub fn allow_wider_children() -> Result<bool, String> {
    read().map(|f| f.delegation.allow_wider_children)
}

/// **The ceilings every node starts under**, from [`path`]: the operator's where stated, else
/// [`crate::node_limits`]'s defaults.
pub fn node_limits() -> Result<crate::node_limits::NodeLimits, String> {
    read().map(|f| limits_of(&f))
}

fn limits_of(f: &File) -> crate::node_limits::NodeLimits {
    let defaults = crate::node_limits::NodeLimits::default();
    crate::node_limits::NodeLimits {
        open_files: f.limits.max_open_files.unwrap_or(defaults.open_files),
        processes: f.limits.max_processes.unwrap_or(defaults.processes),
    }
}

/// The file, parsed. No file is every default; one that cannot be read or parsed is an error
/// naming it.
fn read() -> Result<File, String> {
    let Some(path) = path() else {
        return Ok(File::default());
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(File::default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    toml::from_str::<File>(&text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
fn parse(text: &str) -> Result<bool, toml::de::Error> {
    toml::from_str::<File>(text).map(|f| f.delegation.allow_wider_children)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_opt_in_is_off_unless_the_operator_states_it() {
        assert!(!parse("").unwrap());
        assert!(!parse("[delegation]\n").unwrap());
        assert!(parse("[delegation]\nallow_wider_children = true\n").unwrap());
        assert!(
            parse("[delegation]\nallow_wider_childen = true\n").is_err(),
            "a misspelt key is an error, never a silent default"
        );
    }

    #[test]
    fn the_operator_raises_a_node_ceiling_and_the_other_keeps_its_default() {
        let f: File = toml::from_str("[limits]\nmax_open_files = 16384\n").unwrap();
        let limits = limits_of(&f);
        assert_eq!(limits.open_files, 16384);
        assert_eq!(limits.processes, crate::node_limits::DEFAULT_PROCESSES);
        assert!(toml::from_str::<File>("[limits]\nmax_files = 1\n").is_err());
    }
}
