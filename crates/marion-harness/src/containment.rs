//! **How far a node is contained, and the rule that a spawn may not loosen it.**
//!
//! A harness either runs its tools inside an OS sandbox of its own (codex's `workspace-write` and
//! `read-only` seatbelt/landlock modes) or it does not, and then a node that may write or run a
//! command does so as the operator, anywhere. Each row states which ([`ContainmentRule`]); an
//! agent type's grants then place a node on one ordered scale ([`Containment`]), and a caller may
//! spawn only a child at least as contained as itself ([`crate::authority::permits`]). Without that, a sandboxed node
//! escapes by delegating: a codex implementer asks for a claude implementer, whose shell is the
//! operator's.

use marion_core::agent_type::AgentType;
use serde::{Deserialize, Serialize};

use crate::adapter::{harness_spec, runs_commands, writes_files};

/// What a row's harness does to contain the node it runs. Row data, stated by every row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainmentRule {
    /// No sandbox marion can rely on: a node that may write files or run commands does so as the
    /// operator; one granted neither is read-only.
    ToolsOnly,
    /// The harness's own OS sandbox bounds writes and commands to the workspace whenever it grants
    /// writing, and to reading otherwise. `verify` is the argv, after the row's program and its
    /// update switch, that runs a command inside the same `workspace-write` sandbox — how a
    /// child's verification lines are kept to its workspace too.
    HarnessSandbox { verify: &'static [&'static str] },
}

/// Where a node stands, least contained first, so `a < b` reads "a is less contained than b".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Containment {
    /// Writes and commands as the operator, anywhere the operator can.
    Uncontained,
    /// Writes and commands inside its workspace, under the harness's sandbox.
    WorkspaceWrites,
    /// Neither writes nor runs anything that changes state.
    ReadOnly,
}

impl std::fmt::Display for Containment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Containment::Uncontained => "uncontained (writes and commands as the operator)",
            Containment::WorkspaceWrites => "sandboxed to its workspace",
            Containment::ReadOnly => "read-only",
        })
    }
}

impl Containment {
    /// What a child at this position would be, said as the reason its caller cannot start it.
    pub fn as_child(self) -> &'static str {
        match self {
            Containment::Uncontained => "has no sandbox marion can apply",
            Containment::WorkspaceWrites => "would run with a looser sandbox",
            Containment::ReadOnly => "is read-only",
        }
    }
}

/// **Where marion's own OS sandbox would bound a node** ([`crate::os_sandbox`]): the auth its
/// tree runs under, and whether the sandbox is on and supported on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Host {
    pub auth: crate::Auth,
    pub os_sandbox: bool,
}

impl Host {
    /// This host, for a tree under `auth` whose supervisor has the sandbox `enabled`
    /// (`run::Env::os_sandbox`): on only where the host also supports it.
    pub fn here(auth: crate::Auth, enabled: bool) -> Host {
        Host {
            auth,
            os_sandbox: enabled && crate::os_sandbox::support().available(),
        }
    }

    /// Whether marion's sandbox bounds a node of `t` here — the same decision the launch makes
    /// ([`crate::os_sandbox::applies`]), so no node is labelled by one rule and run by another.
    fn contains(self, t: &AgentType) -> bool {
        self.os_sandbox
            && crate::os_sandbox::covers(
                harness_spec(t.harness).os_sandbox,
                self.auth,
                !writes_files(t),
            )
    }
}

/// Where a node of type `t` stands on `host`: kept to its workspace (or read-only) by marion's
/// own sandbox where it applies, else by its row's rule and its grants alone ([`of`]).
pub fn on(t: &AgentType, host: Host) -> Containment {
    if host.contains(t) {
        return if writes_files(t) || runs_commands(t) {
            Containment::WorkspaceWrites
        } else {
            Containment::ReadOnly
        };
    }
    of(t)
}

/// Whether a node of `t` is kept in a sandbox on `host` — its harness's own, or marion's where
/// it bounds something the node does. A node read-only by its grants is not sandboxed by marion's:
/// it chose not to write, and delegating writes is what it is for ([`sandboxed`]).
pub fn sandboxed_on(t: &AgentType, host: Host) -> bool {
    sandboxed(t) || (host.contains(t) && (writes_files(t) || runs_commands(t)))
}

/// Where a node of type `t` stands, from its row's rule and its grants.
pub fn of(t: &AgentType) -> Containment {
    let acts = writes_files(t) || runs_commands(t);
    match harness_spec(t.harness).containment {
        ContainmentRule::ToolsOnly if acts => Containment::Uncontained,
        ContainmentRule::HarnessSandbox { .. } if writes_files(t) => Containment::WorkspaceWrites,
        _ => Containment::ReadOnly,
    }
}

/// The argv that runs a verification line inside `t`'s harness sandbox, after the row's program:
/// the update switch, the row's `verify` prefix. `None` where the row has no sandbox to run it in.
pub fn verify_prefix(t: &AgentType) -> Option<(&'static str, Vec<String>)> {
    let spec = harness_spec(t.harness);
    let ContainmentRule::HarnessSandbox { verify } = spec.containment else {
        return None;
    };
    let program = spec.program?;
    let mut argv = Vec::new();
    if let Some((key, value)) = spec.updates.pair() {
        argv.push("-c".to_string());
        argv.push(format!("{key}={value}"));
    }
    argv.extend(verify.iter().map(|a| a.to_string()));
    Some((program, argv))
}

/// Whether `t`'s harness keeps it in a sandbox of its own — the containment a spawn must not
/// loosen. A node read-only by its grants alone (an orchestrator on a row with no sandbox) is not
/// sandboxed: it chose not to write, and delegating writes is what it is for.
pub fn sandboxed(t: &AgentType) -> bool {
    matches!(
        harness_spec(t.harness).containment,
        ContainmentRule::HarnessSandbox { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::agent_type::builtin;

    fn at(name: &str) -> Containment {
        of(&builtin(name).unwrap_or_else(|| panic!("{name} is built in")))
    }

    /// **Where each built-in stands**: codex's implementer is kept to its workspace by codex's own
    /// sandbox, an orchestrator that neither writes nor runs anything is read-only on any row, and
    /// an implementer on a row with no sandbox is uncontained.
    #[test]
    fn each_built_in_type_stands_where_its_row_and_grants_put_it() {
        assert_eq!(at("codex"), Containment::WorkspaceWrites);
        assert_eq!(at("claude-orchestrator"), Containment::ReadOnly);
        assert_eq!(at("claude"), Containment::Uncontained);
        assert!(Containment::Uncontained < Containment::WorkspaceWrites);
        assert!(Containment::WorkspaceWrites < Containment::ReadOnly);
    }

    fn on_host(name: &str, auth: crate::Auth, os_sandbox: bool) -> Containment {
        on(
            &builtin(name).unwrap_or_else(|| panic!("{name} is built in")),
            Host { auth, os_sandbox },
        )
    }

    /// **Under marion's own sandbox a wrapped row's implementer is contained to its workspace**
    /// — on a host where it is on, for a node whose harness home is marion's. Under the operator's
    /// own login, or where the sandbox is off or unsupported, it stands where its row alone puts
    /// it; codex is contained either way.
    #[test]
    fn marions_sandbox_contains_a_wrapped_rows_implementer_where_it_applies() {
        use crate::Auth::{Canned, Endpoint, Inherited};
        assert_eq!(
            on_host("claude", Canned, true),
            Containment::WorkspaceWrites
        );
        assert_eq!(
            on_host("claude", Endpoint, true),
            Containment::WorkspaceWrites
        );
        assert_eq!(on_host("claude", Inherited, true), Containment::Uncontained);
        assert_eq!(on_host("claude", Canned, false), Containment::Uncontained);
        assert_eq!(on_host("codex", Canned, true), Containment::WorkspaceWrites);
        assert_eq!(
            on_host("codex", Inherited, false),
            Containment::WorkspaceWrites
        );
        assert_eq!(
            on_host("claude-orchestrator", Canned, true),
            Containment::ReadOnly
        );
    }

    /// Only a row with a sandbox offers one to run verification in, and it carries the row's own
    /// update switch so the probe cannot update the operator's install.
    #[test]
    fn only_a_sandboxed_row_offers_a_verification_prefix() {
        let codex = verify_prefix(&builtin("codex").unwrap()).expect("codex has a sandbox");
        assert_eq!(codex.0, "codex");
        assert_eq!(
            codex.1[..2],
            ["-c".to_string(), "check_for_update_on_startup=false".into()]
        );
        assert!(codex.1.ends_with(&["--".to_string()]));
        assert_eq!(verify_prefix(&builtin("claude").unwrap()), None);
    }
}
