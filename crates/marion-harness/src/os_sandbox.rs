//! **marion's own OS sandbox around a node**: writes only to the node's workspace, its `TMPDIR`
//! and its agent dir, plus the few paths its row measured it needs — macOS Seatbelt through
//! `sandbox-exec`, Linux Landlock at ABI 2 or later. Reads stay broad.
//!
//! Every row states how the sandbox meets its harness ([`OsSandboxRule`]): marion **wraps** a
//! harness with no sandbox of its own on by default; it **replaces** one whose own sandbox cannot
//! run inside marion's (the kernel refuses a nested Seatbelt, measured on codex); and a row nobody
//! has measured under the profile is **unsupported**, so it keeps the containment it had. Nothing
//! here is per harness: a row's strategy and its extra write paths are data.
//!
//! Phase 1 applies the sandbox only where a node's harness home is marion's own — canned and
//! endpoint auth, where the home lives in the agent dir. Under the operator's own login a
//! harness writes its real home, and a writable home is a way out of the sandbox (a hook the
//! operator's next unsandboxed session runs), so those nodes keep their old label until each
//! row's home rule is narrowed and measured.

/// How marion's sandbox meets one row's harness. Row data, stated by every row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsSandboxRule {
    /// The harness has no sandbox of its own on by default: marion's profile wraps its process.
    /// `writes` are the paths beyond the node's own that it measured it needs.
    Wrap { writes: &'static [WritePath] },
    /// The harness's own sandbox cannot run inside marion's, so it is switched off where marion's
    /// applies: `off` is `(field, JSON value)` on the thread's opening request, which beats every
    /// other switch the harness reads. A read-only node keeps the harness's own read-only sandbox
    /// instead, which needs no replacing.
    ReplaceOwn {
        writes: &'static [WritePath],
        off: &'static [(&'static str, &'static str)],
    },
    /// Not applied, and why: the node keeps the containment its row had without it.
    Unsupported { why: &'static str },
}

/// A path a harness writes beyond its node's own dirs, relative to the **node's** home: the
/// `HOME` its launch sets where it sets one (a canned home in the agent dir), else the operator's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritePath {
    /// `$HOME/<rel>`.
    Home(&'static str),
    /// `$HOME/<rel>/<the node's cwd, every byte not ASCII alphanumeric as '-'>` — one project's
    /// directory under a harness home keyed on the working directory (claude's `projects/`).
    HomeProject(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::harness::Harness;

    /// **The sweep: every row states how marion's sandbox meets its harness**, and says why where
    /// it does not apply. The rows measured under the profile are wrapped, codex's own sandbox is
    /// replaced, and every extra write path is relative to a home — never an absolute path that
    /// would open the same directory to every operator's nodes alike.
    #[test]
    fn every_row_states_its_sandbox_strategy() {
        for h in Harness::ALL {
            let rule = crate::adapter::harness_spec(h).os_sandbox;
            let writes: &[WritePath] = match rule {
                OsSandboxRule::Wrap { writes } | OsSandboxRule::ReplaceOwn { writes, .. } => writes,
                OsSandboxRule::Unsupported { why } => {
                    assert!(
                        !why.trim().is_empty(),
                        "{h:?} must say why it is not sandboxed"
                    );
                    &[]
                }
            };
            for w in writes {
                let (WritePath::Home(rel) | WritePath::HomeProject(rel)) = w;
                assert!(
                    !rel.starts_with('/') && !rel.contains(".."),
                    "{h:?}: `{rel}` must stay under the node's home"
                );
            }
        }
        let wrapped = |h| {
            matches!(
                crate::adapter::harness_spec(h).os_sandbox,
                OsSandboxRule::Wrap { .. }
            )
        };
        for h in [
            Harness::ClaudeCode,
            Harness::Gemini,
            Harness::OpenCode,
            Harness::Copilot,
            Harness::Goose,
            Harness::Qwen,
            Harness::Pi,
        ] {
            assert!(wrapped(h), "{h:?} was measured under the profile");
        }
        assert!(matches!(
            crate::adapter::harness_spec(Harness::Codex).os_sandbox,
            OsSandboxRule::ReplaceOwn { .. }
        ));
    }
}
