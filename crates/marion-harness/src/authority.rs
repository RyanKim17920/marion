//! **A child never receives more authority than its parent holds, on any axis.**
//!
//! A node delegates by naming an agent type, and that type decides what the child may do. Were a
//! type free to grant more than its caller holds, every limit on a node would be one `spawn` away
//! from gone: a read-only planner starts an implementer, a node without a shell starts one with a
//! shell, a sandboxed codex starts an unsandboxed claude. So one rule, on every axis, compares the
//! caller's authority with the child's ([`permits`]):
//!
//! * **write** — a parent that cannot write files cannot start a child that can;
//! * **shell** — a parent that cannot run commands cannot start a child that can;
//! * **read-only** — the two above together: a parent that can do neither (a planner, an
//!   orchestrator) starts only children that can do neither;
//! * **containment** — a parent its harness sandboxes cannot start a less-contained child
//!   ([`crate::containment`]);
//! * **approval mode** — a child cannot run in an approval mode its parent does not;
//! * **writable scope** — a child's scope ceiling stays inside its parent's ([`permits_scope`] does
//!   the same for a spawn's requested scope against what its caller was granted).
//!
//! A node's authority is read from its resolved agent type and its row; a root's is the type the
//! operator launched. The operator, and only the operator, can widen it deliberately for a tree
//! (user-level config or `marion run --allow-wider-children`), and then every widened delegation
//! is journaled.

use marion_core::agent_type::AgentType;
use marion_core::contract::Glob;

use crate::adapter::{runs_commands, writes_files};
use crate::containment::{self, Containment};

/// **A launch flag that puts a harness session in a read-only mode**, as a row states it: any of
/// `flags` followed by (or `=`-joined to) any of `values`. `marion claude --permission-mode plan`
/// is claude's; `marion codex --sandbox read-only` is codex's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadOnlyMode {
    pub flags: &'static [&'static str],
    pub values: &'static [&'static str],
}

/// **Whether a session launched with `argv` runs in one of its row's read-only modes**: what the
/// operator's own flags on `marion <harness> …` say, as far as marion can see them. A mode chosen
/// inside the session afterwards (claude's shift-tab) or set in the harness's own config files is
/// not visible here.
pub fn session_read_only(modes: &[ReadOnlyMode], argv: &[std::ffi::OsString]) -> bool {
    let args: Vec<&str> = argv.iter().filter_map(|a| a.to_str()).collect();
    args.iter().enumerate().any(|(i, arg)| {
        modes.iter().any(|m| {
            m.flags.iter().any(|flag| {
                let joined = arg
                    .strip_prefix(flag)
                    .and_then(|rest| rest.strip_prefix('='));
                let next = (arg == flag).then(|| args.get(i + 1).copied()).flatten();
                joined.or(next).is_some_and(|v| m.values.contains(&v))
            })
        })
    })
}

/// One axis a child's authority is compared with its parent's on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Axis {
    ReadOnly,
    Write,
    Shell,
    Containment,
    ApprovalMode,
    WritableScope,
}

impl Axis {
    /// The axis as the journal and a node's row name it.
    pub fn as_str(self) -> &'static str {
        match self {
            Axis::ReadOnly => "read-only",
            Axis::Write => "write",
            Axis::Shell => "shell",
            Axis::Containment => "containment",
            Axis::ApprovalMode => "approval-mode",
            Axis::WritableScope => "writable-scope",
        }
    }
}

/// What a node may do, read off its agent type and row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority {
    pub write: bool,
    pub shell: bool,
    pub containment: Containment,
    /// Whether its harness sandboxes it, so [`Self::containment`] is a limit it holds rather than a
    /// choice its grants made.
    pub sandboxed: bool,
    pub approval_mode: Option<String>,
    pub scope_ceiling: Vec<Glob>,
    /// A planner that may start children that write or run commands although it does neither
    /// ([`AgentType::delegates_writes`], marion-owned).
    pub delegates_writes: bool,
}

impl Authority {
    /// The authority a node of type `t` holds.
    pub fn of(t: &AgentType) -> Authority {
        Authority {
            write: writes_files(t),
            shell: runs_commands(t),
            containment: containment::of(t),
            sandboxed: containment::sandboxed(t),
            approval_mode: t.approval_mode.clone(),
            scope_ceiling: t.scope_ceiling.clone(),
            delegates_writes: t.delegates_writes,
        }
    }

    /// This authority for a node marion knows to be read-only whatever its type grants — a session
    /// the operator started in a read-only mode (claude's `plan`, codex's `read-only` sandbox), or
    /// a reviewer: it writes and runs nothing, delegates nothing that does, and is contained as a
    /// read-only node is.
    pub fn in_read_only_session(self) -> Authority {
        Authority {
            write: false,
            shell: false,
            containment: Containment::ReadOnly,
            delegates_writes: false,
            ..self
        }
    }

    fn read_only(&self) -> bool {
        !self.write && !self.shell
    }
}

/// Why a child was refused: the axis, the two types, and a sentence naming what differs and how
/// the operator can allow it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// Every axis the child exceeds its parent on, in [`Axis`] order; the first leads the sentence.
    pub axes: Vec<Axis>,
    pub parent: String,
    pub child: String,
    pub why: String,
}

/// What the operator does to allow a wider child: one sentence, shared by every refusal.
pub const FIX: &str = "To allow this, set `[delegation] allow_wider_children = true` in \
                       ~/.config/marion/config.toml, or run `marion run --allow-wider-children`.";

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {FIX}", self.why)
    }
}

/// **Whether a node of type `parent` may start a child of type `child`**: `Ok` when the child
/// holds no more than the parent on every axis, else a [`Refusal`] naming each axis it exceeds.
/// Pure: the types and their rows decide it, nothing else.
pub fn permits(parent: &AgentType, child: &AgentType) -> Result<(), Refusal> {
    permits_from(&Authority::of(parent), &parent.name, child)
}

/// [`permits`] over a parent's [`Authority`] as it stands — its type's, or less where the
/// operator started its session in a read-only mode ([`Authority::in_read_only_session`]).
pub fn permits_from(p: &Authority, parent: &str, child: &AgentType) -> Result<(), Refusal> {
    permits_between(p, parent, &Authority::of(child), &child.name)
}

/// [`permits`] over two authorities as they stand — a child's is less than its type's where it
/// is a reviewer, which marion runs read-only whatever its type.
pub fn permits_between(
    p: &Authority,
    parent: &str,
    c: &Authority,
    child: &str,
) -> Result<(), Refusal> {
    let (pn, cn) = (parent, child);
    let mut axes = Vec::new();
    let mut why = Vec::new();
    if p.delegates_writes {
        // A planner built to delegate: what its children write and run is bounded by the other
        // axes, never by its own choice not to write.
    } else if p.read_only() && !c.read_only() {
        axes.push(Axis::ReadOnly);
        why.push(format!(
            "{pn} is read-only, so it can start only read-only agents; {cn} can write files or run \
             commands."
        ));
    } else {
        if c.write && !p.write {
            axes.push(Axis::Write);
            why.push(format!(
                "{pn} cannot write files, so it can't start {cn}, which can."
            ));
        }
        if c.shell && !p.shell {
            axes.push(Axis::Shell);
            why.push(format!(
                "{pn} cannot run commands, so it can't start {cn}, which can."
            ));
        }
    }
    if p.sandboxed && c.containment < p.containment {
        axes.push(Axis::Containment);
        why.push(format!(
            "{pn} runs sandboxed here; {cn} {}, so a {pn} agent can't start it.",
            c.containment.as_child()
        ));
    }
    if c.approval_mode.is_some() && c.approval_mode != p.approval_mode {
        axes.push(Axis::ApprovalMode);
        why.push(format!(
            "{cn} runs in approval mode `{}`, which {pn} does not have.",
            c.approval_mode.as_deref().unwrap_or_default()
        ));
    }
    if permits_scope(&p.scope_ceiling, &c.scope_ceiling).is_err() {
        axes.push(Axis::WritableScope);
        why.push(format!("{cn} may write outside the paths {pn} may write."));
    }
    if axes.is_empty() {
        Ok(())
    } else {
        Err(Refusal {
            axes,
            parent: pn.to_string(),
            child: cn.to_string(),
            why: why.join(" "),
        })
    }
}

/// **Whether a child's scope stays inside its parent's**: every glob of `child` covered by some
/// glob of `parent`. `Err` names the first that is not.
///
/// Decided textually and in the safe direction: a parent glob covers a child glob when it is `**`,
/// the same glob, or `<dir>/**` over a child glob under `<dir>/`. Anything else — two unrelated
/// wildcard patterns whose overlap only a filesystem could settle — is refused, since a
/// wrongly-admitted scope is a child writing where its parent could not, and a wrongly-refused one
/// is a spawn that names a narrower scope.
pub fn permits_scope(parent: &[Glob], child: &[Glob]) -> Result<(), String> {
    let covers = |p: &str, c: &str| {
        p == "**"
            || p == c
            || p.strip_suffix("/**")
                .is_some_and(|dir| c.starts_with(&format!("{dir}/")) && !c.contains(".."))
    };
    match child
        .iter()
        .find(|c| !parent.iter().any(|p| covers(&p.0, &c.0)))
    {
        None => Ok(()),
        Some(c) => Err(format!("`{}` is outside the parent's scope", c.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::agent_type::{builtin, builtin_names};

    /// **The sweep: every pair of built-in types**, each judged against the rule stated axis by
    /// axis from the two types alone. A pair is permitted exactly when the child exceeds its parent
    /// on no axis, and a refusal names every axis it does.
    #[test]
    fn every_pair_of_built_in_types_is_judged_axis_by_axis() {
        let names = builtin_names();
        assert!(names.len() > 5, "the sweep covers the whole table");
        for pn in names.iter() {
            for cn in names.iter() {
                let (parent, child) = (builtin(pn).unwrap(), builtin(cn).unwrap());
                let (p, c) = (Authority::of(&parent), Authority::of(&child));
                let mut expect = Vec::new();
                if p.delegates_writes {
                } else if p.read_only() && !c.read_only() {
                    expect.push(Axis::ReadOnly);
                } else {
                    if c.write && !p.write {
                        expect.push(Axis::Write);
                    }
                    if c.shell && !p.shell {
                        expect.push(Axis::Shell);
                    }
                }
                if p.sandboxed && c.containment < p.containment {
                    expect.push(Axis::Containment);
                }
                if c.approval_mode.is_some() && c.approval_mode != p.approval_mode {
                    expect.push(Axis::ApprovalMode);
                }
                if permits_scope(&p.scope_ceiling, &c.scope_ceiling).is_err() {
                    expect.push(Axis::WritableScope);
                }
                match permits(&parent, &child) {
                    Ok(()) => assert!(expect.is_empty(), "{pn} -> {cn} permitted over {expect:?}"),
                    Err(r) => {
                        assert_eq!(r.axes, expect, "{pn} -> {cn}");
                        let said = r.to_string();
                        assert!(said.contains(*pn) && said.contains(*cn), "{said}");
                        assert!(said.ends_with(FIX), "{said}");
                    }
                }
                // A type may always start its own kind.
                if pn == cn {
                    assert!(permits(&parent, &child).is_ok(), "{pn} -> itself");
                }
            }
        }
    }

    /// The axes, one by one, on types that differ on exactly that axis.
    #[test]
    fn each_axis_refuses_by_name() {
        let ty = |n: &str| builtin(n).unwrap();
        let axes = |p: &str, c: &str| permits(&ty(p), &ty(c)).err().map(|r| r.axes);
        // The built-in planners delegate writes; a read-only type that is not one does not.
        assert_eq!(axes("claude-orchestrator", "claude"), None);
        assert_eq!(axes("claude-orchestrator", "codex"), None);
        let mut reader = ty("claude-orchestrator");
        reader.delegates_writes = false;
        assert_eq!(
            permits(&reader, &ty("claude")).err().map(|r| r.axes),
            Some(vec![Axis::ReadOnly])
        );
        // A planner is still bounded on the other axes.
        let mut narrow_planner = ty("claude-orchestrator");
        narrow_planner.scope_ceiling = vec![Glob("src/**".into())];
        assert_eq!(
            permits(&narrow_planner, &ty("codex")).err().map(|r| r.axes),
            Some(vec![Axis::WritableScope])
        );
        assert_eq!(axes("codex", "claude"), Some(vec![Axis::Containment]));
        assert_eq!(axes("claude", "codex"), None);
        assert_eq!(axes("claude", "claude-orchestrator"), None);
        assert_eq!(axes("codex", "claude-orchestrator"), None);

        let mut shell_only = ty("claude");
        shell_only.tools = vec!["read".into(), "bash".into()];
        let mut write_only = ty("claude");
        write_only.tools = vec!["read".into(), "write".into(), "edit".into()];
        let r = permits(&write_only, &shell_only).unwrap_err();
        assert_eq!(r.axes, vec![Axis::Shell]);
        assert!(r.why.contains("cannot run commands"), "{}", r.why);

        let mut moded = ty("acp-qwen");
        moded.approval_mode = Some("agent-full-access".into());
        let r = permits(&ty("acp-qwen"), &moded).unwrap_err();
        assert_eq!(r.axes, vec![Axis::ApprovalMode]);
        assert!(permits(&moded, &moded).is_ok());

        let mut narrow = ty("claude");
        narrow.scope_ceiling = vec![Glob("src/**".into())];
        let r = permits(&narrow, &ty("claude")).unwrap_err();
        assert_eq!(r.axes, vec![Axis::WritableScope]);
        assert!(permits(&ty("claude"), &narrow).is_ok());
    }

    /// **A read-only launch is read from the operator's flags, as each row states them**, in
    /// both spellings, and nothing else reads as one.
    #[test]
    fn a_read_only_session_is_read_from_the_rows_own_flags() {
        let os = |v: &[&str]| v.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        let claude =
            crate::adapter::harness_spec(marion_core::harness::Harness::ClaudeCode).read_only_modes;
        assert!(session_read_only(
            claude,
            &os(&["--permission-mode", "plan"])
        ));
        assert!(session_read_only(
            claude,
            &os(&["--model", "x", "--permission-mode=plan"])
        ));
        assert!(!session_read_only(
            claude,
            &os(&["--permission-mode", "acceptEdits"])
        ));
        assert!(!session_read_only(
            claude,
            &os(&["plan", "--permission-mode"])
        ));
        let codex =
            crate::adapter::harness_spec(marion_core::harness::Harness::Codex).read_only_modes;
        assert!(session_read_only(codex, &os(&["-s", "read-only"])));
        assert!(session_read_only(codex, &os(&["--sandbox=read-only"])));
        assert!(!session_read_only(
            codex,
            &os(&["--sandbox", "workspace-write"])
        ));

        // Such a session is a read-only non-delegator, even on claude's implementer type.
        let planning = Authority::of(&builtin("claude").unwrap()).in_read_only_session();
        let r = permits_from(&planning, "claude", &builtin("codex").unwrap()).unwrap_err();
        assert_eq!(r.axes, vec![Axis::ReadOnly]);
        assert!(
            permits_from(
                &planning,
                "claude",
                &builtin("claude-orchestrator").unwrap()
            )
            .is_ok()
        );
    }

    #[test]
    fn a_scope_stays_inside_its_parents() {
        let g = |v: &[&str]| v.iter().map(|s| Glob(s.to_string())).collect::<Vec<_>>();
        assert!(permits_scope(&g(&["**"]), &g(&["src/**", "docs/*.md"])).is_ok());
        assert!(permits_scope(&g(&["src/**"]), &g(&["src/a/**", "src/b.rs"])).is_ok());
        assert!(permits_scope(&g(&["src/**"]), &g(&["**"])).is_err());
        assert!(permits_scope(&g(&["src/**"]), &g(&["docs/**"])).is_err());
        assert!(permits_scope(&g(&["src/**"]), &g(&["src/../etc/**"])).is_err());
        assert!(permits_scope(&g(&["src/*.rs"]), &g(&["src/*.rs"])).is_ok());
    }
}
