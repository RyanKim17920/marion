//! The Claude Code adapter (design §5.2).
//!
//! Control is **config-time**: marion owns the launch configuration and never parses a pty. Every
//! flag here was measured against 2.1.220, and several are non-obvious enough that the tests below
//! state *why* rather than merely pinning the string.

use std::path::PathBuf;

/// An argv + env pair, ready to spawn. Nothing here reaches a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// What marion needs to compile a headless root invocation.
#[derive(Debug, Clone)]
pub struct HeadlessSpec {
    pub cwd: PathBuf,
    pub model: Option<String>,
    /// Tools the root may call **without a prompt** — the permission axis. marion's own tools go
    /// here, or the root's single load-bearing `spawn` call is denied.
    pub allowed_tools: Vec<String>,
    /// Path to the MCP server declaration marion wrote.
    pub mcp_config: PathBuf,
    /// Base URL for the canned provider.
    pub base_url: Option<String>,
}

pub fn compile_headless(spec: &HeadlessSpec) -> Invocation {
    let mut args: Vec<String> = vec![
        "-p".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--input-format".into(),
        "stream-json".into(),
        // MANDATORY with `-p --output-format stream-json`: without it 2.1.220 exits 1 with
        // "When using --print, --output-format=stream-json requires --verbose" before emitting
        // anything. Easy to omit, because this document long explained it only as a dependency of
        // --include-partial-messages, which M1 does not use.
        "--verbose".into(),
        // Availability axis: no built-in tools. This does NOT gate MCP tools.
        "--tools".into(),
        "".into(),
        // Permission axis. marion's tools must be listed here or a denied `spawn` is the result.
        "--allowedTools".into(),
        spec.allowed_tools.join(","),
        // Without this, a non-allowlisted call is auto-denied in-process and surfaces only as an
        // is_error tool_result — no `can_use_tool` frame ever reaches marion. Absent from --help.
        "--permission-prompt-tool".into(),
        "stdio".into(),
        // Only the MCP servers marion declared; never the user's.
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        spec.mcp_config.to_string_lossy().into_owned(),
        // No user settings leak into a child. Note this does NOT suppress the session-title
        // request (§5.5).
        "--setting-sources".into(),
        "".into(),
    ];
    if let Some(m) = &spec.model {
        args.push("--model".into());
        args.push(m.clone());
    }

    let mut env = Vec::new();
    if let Some(u) = &spec.base_url {
        env.push(("ANTHROPIC_BASE_URL".to_string(), u.clone()));
    }
    // NOT CLAUDE_CONFIG_DIR: isolating it breaks OAuth, because the Keychain entry is keyed to the
    // real config dir. Config isolation and subscription auth are mutually exclusive here, and the
    // fileless path (--mcp-config + --setting-sources "") is what keeps auth working.

    Invocation {
        program: "claude".into(),
        args,
        env,
        cwd: spec.cwd.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> HeadlessSpec {
        HeadlessSpec {
            cwd: "/tmp/wt".into(),
            model: Some("haiku".into()),
            allowed_tools: vec!["mcp__marion__spawn".into(), "mcp__marion__status".into()],
            mcp_config: "/tmp/mcp.json".into(),
            base_url: Some("http://127.0.0.1:8099".into()),
        }
    }

    fn args_of(inv: &Invocation) -> String {
        inv.args.join(" ")
    }

    #[test]
    fn verbose_is_present_because_the_cli_exits_1_without_it() {
        let inv = compile_headless(&spec());
        assert!(
            inv.args.iter().any(|a| a == "--verbose"),
            "2.1.220 refuses -p --output-format stream-json without --verbose"
        );
    }

    #[test]
    fn the_two_tool_axes_are_distinct() {
        let inv = compile_headless(&spec());
        let s = args_of(&inv);
        // --tools is availability and does NOT gate MCP tools; --allowedTools is permission and
        // is what a denied mcp__marion__spawn call turns on.
        assert!(s.contains("--tools  --allowedTools") || s.contains("--tools "));
        let i = inv.args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(inv.args[i + 1], "mcp__marion__spawn,mcp__marion__status");
    }

    #[test]
    fn permission_prompt_tool_is_set_or_can_use_tool_never_fires() {
        let inv = compile_headless(&spec());
        let i = inv
            .args
            .iter()
            .position(|a| a == "--permission-prompt-tool")
            .expect("without this the CLI auto-denies in-process and marion sees nothing");
        assert_eq!(inv.args[i + 1], "stdio");
    }

    #[test]
    fn mcp_config_is_strict_so_the_users_servers_stay_out() {
        let inv = compile_headless(&spec());
        assert!(inv.args.iter().any(|a| a == "--strict-mcp-config"));
        assert!(inv.args.iter().any(|a| a == "--mcp-config"));
    }

    #[test]
    fn claude_config_dir_is_never_isolated() {
        let inv = compile_headless(&spec());
        assert!(
            !inv.env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"),
            "isolating it breaks OAuth: the Keychain entry is keyed to the real config dir"
        );
    }

    #[test]
    fn nothing_is_shell_quoted_because_nothing_reaches_a_shell() {
        let inv = compile_headless(&spec());
        assert_eq!(inv.program, "claude");
        assert!(
            inv.args
                .iter()
                .all(|a| !a.contains('\'') && !a.contains('"'))
        );
    }
}
