//! The minimum honest agent-type registry for M1 (§3.1 key table, §6.1 step 2, §9).
//!
//! §3.1's full key set is a file format with discovery and precedence rules; none of that is M1.
//! What M1 genuinely needs is the handful of per-type values that other §-rules already read:
//! the writable-scope ceiling (§5.4), the node's timeout bound (§9), and the two `spawn` gates
//! (§6.1 step 2). So this is a plain lookup over two built-in types, not a loader — a loader with
//! no file format behind it would be scaffolding pretending to be a feature.
//!
//! Everything here is pure data plus one predicate; nothing reads a file.

use crate::contract::Glob;
use crate::encoding::Duration;
use crate::harness::Harness;

/// §3.1: `timeout_secs` defaults to 900 s, and §9 re-resolves it fresh on resume.
pub const DEFAULT_TIMEOUT_SECS: u64 = 900;
/// §3.1/§6.1 step 2: depth gate, counting the root as 0.
pub const DEFAULT_MAX_DEPTH: u32 = 3;
/// §3.1/§6.1 step 2: live (non-terminal, unreaped) children of *this* node.
pub const DEFAULT_MAX_CONCURRENT_CHILDREN: u32 = 4;

/// The `gemini` built-in's default model.
///
/// The one id S12 exercised end to end against a canned endpoint. It is a *default*, not a claim
/// about which model runs: S12 measured 0.53.0 rewriting an explicit `-m gemini-2.5-flash` to
/// `gemini-3.5-flash` in the request path, which is precisely why the contract records what the
/// adapter compiled rather than treating the request as an outcome.
pub const GEMINI_DEFAULT_MODEL: &str = "gemini-2.5-flash";

/// The `opencode` built-in's default model, in the `provider/model` form that is the only spelling
/// `-m` accepts (S13: there is no `OPENCODE_MODEL` env var, so argv and the generated config are
/// the only two channels).
///
/// Both halves name marion's own plumbing rather than a vendor's catalogue: the adapter *generates*
/// the provider block for whatever provider id this names, pointing it at marion's base URL, so the
/// id is marion's to choose; the model id is what marion's endpoint is then asked for. An operator
/// pointing a node at a real endpoint overrides both through `spawn`'s `model`.
pub const OPENCODE_DEFAULT_MODEL: &str = "marion/default";

/// §5.4/§6.7: an omitted `writable_scope` is **stored** as `["**"]`, never absent, so the
/// conjunction in `scope::Scope` has two lists to work with in every case.
pub fn default_scope_ceiling() -> Vec<Glob> {
    vec![Glob("**".into())]
}

/// The per-type values M1 actually reads. Deliberately not §3.1's full key set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentType {
    /// §3.1: `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$`.
    pub name: String,
    pub description: String,
    /// §3.1 declares `harness` an **enum**, and this is what makes it dispatchable: an adapter is
    /// selected from this value (`marion_harness::adapter_for`), which a `String` could never do
    /// without re-parsing it at every call site.
    pub harness: Harness,
    /// §3.1's `model` key: the type's **default** model, in marion's request vocabulary, which the
    /// harness's adapter maps to that harness's own spelling. `spawn`'s own `model` overrides it.
    ///
    /// Optional rather than required, and `None` on the two harnesses that already run without
    /// one. `codex exec` takes no model argument at all and Claude Code's `--model` is legitimately
    /// omissible, so a default there would either be inert or would change an argv that is
    /// currently measured. The two new harnesses genuinely cannot launch without one — gemini's
    /// `auto` router hung against a canned endpoint, and opencode has no `OPENCODE_MODEL` env var
    /// — so their built-ins state one and their adapters refuse when none arrives.
    pub model: Option<String>,
    /// Ceiling only. `spawn` may narrow it and never widen it (§5.4).
    pub scope_ceiling: Vec<Glob>,
    pub timeout: Duration,
    pub max_depth: u32,
    pub max_concurrent_children: u32,
}

impl AgentType {
    /// A type carrying every §3.1 default, so a built-in only states what it changes.
    fn defaults(name: &str, description: &str, harness: Harness) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            harness,
            // Stated by the types that need one; see the field's doc comment for why the default
            // is an absence rather than a guess.
            model: None,
            scope_ceiling: default_scope_ceiling(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            max_depth: DEFAULT_MAX_DEPTH,
            max_concurrent_children: DEFAULT_MAX_CONCURRENT_CHILDREN,
        }
    }
}

/// §3.1's name rule, checked here so a type name can never become a surprising path or flag.
pub fn is_valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    if b.is_empty() || b.len() > 64 {
        return false;
    }
    b[0].is_ascii_alphanumeric()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
}

/// The two M1 built-ins, by name. `None` is a spawn error (§6.1 step 2 resolves the type first).
///
/// `codex-impl` is the spelling the supervisor already defaults `spawn`'s `agent_type` to, and
/// `codex` is accepted as the same type so the design's own prose reads correctly; both resolve to
/// one definition rather than two that could drift.
pub fn builtin(name: &str) -> Option<AgentType> {
    match name {
        "claude" => Some(AgentType::defaults(
            "claude",
            "Root orchestrator: plans, delegates, and reviews.",
            Harness::ClaudeCode,
        )),
        "codex" | "codex-impl" => Some(AgentType {
            // The canonical name is the one the supervisor writes into contracts today.
            ..AgentType::defaults(
                "codex-impl",
                "Implements a well-specified change.",
                Harness::Codex,
            )
        }),
        // The two harnesses added with M-generality's adapters. Named for their harness because
        // that is all they are: the same defaults, dispatched elsewhere. Without a built-in name a
        // harness with a working adapter is still unreachable from `spawn`, which resolves an
        // agent *type*, never a harness.
        "gemini" => Some(AgentType {
            model: Some(GEMINI_DEFAULT_MODEL.into()),
            ..AgentType::defaults(
                "gemini",
                "Implements a well-specified change on the Gemini CLI.",
                Harness::Gemini,
            )
        }),
        "opencode" => Some(AgentType {
            model: Some(OPENCODE_DEFAULT_MODEL.into()),
            ..AgentType::defaults(
                "opencode",
                "Implements a well-specified change on opencode.",
                Harness::OpenCode,
            )
        }),
        _ => None,
    }
}

/// Every built-in name, aliases included — what `marion doctor` would list.
pub fn builtin_names() -> &'static [&'static str] {
    &["claude", "codex", "codex-impl", "gemini", "opencode"]
}

/// §6.1 step 2's refusals. Both gates **refuse rather than clamp or queue**: a clamped depth would
/// silently give the parent a shallower tree than it asked for, and a queued `spawn` would block
/// the parent's turn on a bound marion never told it about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SpawnGateError {
    #[error("spawn would create a node at depth {child_depth}, past max_depth {max_depth}")]
    DepthExceeded { child_depth: u32, max_depth: u32 },
    #[error("caller already has {live} live children, at max_concurrent_children {max}")]
    TooManyChildren { live: u32, max: u32 },
}

/// §6.1 step 2, applied to the **caller's** agent type.
///
/// Reading the caller's type, never the child's just-resolved one, is load-bearing: the child has
/// no children yet, so the concurrency gate would be vacuous against its type. `caller_depth` is
/// the caller's own depth with the root at 0, so the new node lands at `caller_depth + 1`.
///
/// A root started by `marion run` never reaches here — §6.1 step 2 says both gates are simply
/// inapplicable to it, since its depth is 0 by definition and it has no parent to count children
/// of. The gates constrain `spawn`, not marion's own start-up of the first node.
pub fn check_spawn_gates(
    caller: &AgentType,
    caller_depth: u32,
    live_children: u32,
) -> Result<(), SpawnGateError> {
    let child_depth = caller_depth + 1;
    if child_depth > caller.max_depth {
        return Err(SpawnGateError::DepthExceeded {
            child_depth,
            max_depth: caller.max_depth,
        });
    }
    if live_children >= caller.max_concurrent_children {
        return Err(SpawnGateError::TooManyChildren {
            live: live_children,
            max: caller.max_concurrent_children,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_carry_the_documented_defaults() {
        for name in builtin_names() {
            let t = builtin(name).expect("listed built-ins must resolve");
            assert_eq!(
                t.timeout,
                Duration::from_secs(900),
                "{name}: §3.1/§9 default"
            );
            assert_eq!(t.max_depth, 3, "{name}");
            assert_eq!(t.max_concurrent_children, 4, "{name}");
            assert_eq!(
                t.scope_ceiling,
                vec![Glob("**".into())],
                "{name}: stored, never absent"
            );
            assert!(is_valid_name(&t.name), "{name}");
        }
        assert!(builtin("nope").is_none());
    }

    #[test]
    fn the_supervisors_default_agent_type_still_resolves() {
        // marion-supervisor defaults `spawn`'s `agent_type` to "codex-impl"; if that stopped
        // resolving, every M1 spawn would fail at §6.1 step 2.
        let t = builtin("codex-impl").expect("codex-impl must exist");
        assert_eq!(t.harness, Harness::Codex);
        assert_eq!(
            builtin("codex"),
            Some(t),
            "the alias must not become a second definition"
        );
    }

    /// Each built-in names the harness its adapter drives. `marion_harness::adapter_for` is what
    /// turns that into behaviour, and `marion-supervisor::run`'s dispatch test asserts the pairing
    /// end to end; here it is just the data.
    #[test]
    fn each_builtin_names_its_own_harness_and_no_name_is_a_second_definition() {
        for (name, h) in [
            ("claude", Harness::ClaudeCode),
            ("codex", Harness::Codex),
            ("codex-impl", Harness::Codex),
            ("gemini", Harness::Gemini),
            ("opencode", Harness::OpenCode),
        ] {
            assert_eq!(builtin(name).unwrap().harness, h, "{name}");
            assert!(builtin_names().contains(&name), "{name} must be listed");
        }
        assert_eq!(
            builtin_names().len(),
            5,
            "a new built-in must be listed here too, or `marion doctor` would not name it"
        );
    }

    /// The two harnesses whose adapters refuse without a model must carry one, and the two that
    /// have always run without one must keep carrying none: a default on `codex` would be inert
    /// (`codex exec` takes no model argument) and one on `claude` would change a measured argv.
    #[test]
    fn only_the_harnesses_that_cannot_launch_without_a_model_state_a_default() {
        assert_eq!(builtin("claude").unwrap().model, None);
        assert_eq!(builtin("codex").unwrap().model, None);
        assert_eq!(builtin("codex-impl").unwrap().model, None);
        assert_eq!(
            builtin("gemini").unwrap().model.as_deref(),
            Some(GEMINI_DEFAULT_MODEL)
        );
        assert_eq!(
            builtin("opencode").unwrap().model.as_deref(),
            Some(OPENCODE_DEFAULT_MODEL)
        );
        // opencode's `-m` accepts nothing else, and the generated provider block has to repeat it.
        assert!(
            OPENCODE_DEFAULT_MODEL
                .split_once('/')
                .is_some_and(|(p, m)| !p.is_empty() && !m.is_empty() && !m.contains('/')),
            "the opencode default must be in `provider/model` form"
        );
    }

    #[test]
    fn a_default_ceiling_is_stored_as_a_glob_not_an_absence() {
        // §6.7 stores `["**"]` rather than `None` so `scope::Scope` always has two lists to
        // conjoin; an absent ceiling would have to be special-cased at every match site.
        let ceiling = default_scope_ceiling();
        let s = crate::scope::Scope::new(&ceiling, &[Glob("src/**".into())]).unwrap();
        assert!(s.is_writable(std::path::Path::new("src/a.rs")));
        assert!(!s.is_writable(std::path::Path::new("docs/a.md")));
    }

    #[test]
    fn the_depth_gate_counts_the_root_as_zero() {
        let t = builtin("claude").unwrap();
        // root(0) → 1 → 2 → 3 are all legal; the node at depth 3 may not spawn a fourth level.
        for d in 0..3 {
            assert!(
                check_spawn_gates(&t, d, 0).is_ok(),
                "depth {d} must be able to spawn"
            );
        }
        assert_eq!(
            check_spawn_gates(&t, 3, 0),
            Err(SpawnGateError::DepthExceeded {
                child_depth: 4,
                max_depth: 3
            }),
            "refused, never silently clamped to depth 3"
        );
    }

    #[test]
    fn the_concurrency_gate_refuses_at_the_bound_not_past_it() {
        let t = builtin("codex-impl").unwrap();
        assert!(
            check_spawn_gates(&t, 0, 3).is_ok(),
            "a 4th live child is still within the bound"
        );
        assert_eq!(
            check_spawn_gates(&t, 0, 4),
            Err(SpawnGateError::TooManyChildren { live: 4, max: 4 }),
            "the 5th is refused, not queued: a queue would block the parent's turn invisibly"
        );
    }

    #[test]
    fn depth_is_checked_before_concurrency() {
        // Both gates failing is one refusal; reporting the depth first keeps the message the one
        // the caller can act on (a deeper tree is never available, a slot eventually is).
        let t = builtin("claude").unwrap();
        assert!(matches!(
            check_spawn_gates(&t, 9, 99),
            Err(SpawnGateError::DepthExceeded { .. })
        ));
    }

    #[test]
    fn gates_read_the_callers_type_so_a_narrow_child_type_cannot_loosen_them() {
        // Passing the child's type here would make the concurrency gate vacuous — the child has no
        // children yet. The signature takes only the caller's type so that mistake cannot compile.
        let strict = AgentType {
            max_concurrent_children: 1,
            ..builtin("claude").unwrap()
        };
        assert!(check_spawn_gates(&strict, 0, 1).is_err());
    }

    #[test]
    fn name_validation_matches_the_documented_pattern() {
        for ok in ["claude", "codex-impl", "a", "A1_b-c", &"x".repeat(64)] {
            assert!(is_valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            "-lead",
            "_lead",
            "has space",
            "has/slash",
            &"x".repeat(65),
        ] {
            assert!(!is_valid_name(bad), "{bad}");
        }
    }
}
