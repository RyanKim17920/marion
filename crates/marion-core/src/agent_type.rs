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

/// The one entry in §3.1's `tools:` vocabulary marion implements today: *may create or overwrite a
/// file*.
///
/// **A vocabulary of one word, on purpose.** §3.1's example line reads `tools: [read, edit, bash]`,
/// and those three are the vocabulary's *shape*, not a catalogue marion has earned. `write` is the
/// only verb whose harness-native mapping has been **measured** on the two harnesses that were
/// blocked (§11 item 24: `claude` 2.1.222 declares `Write` under `--tools "Write"`; `gemini` 0.53.0
/// declares `write_file` under `--approval-mode auto_edit`) — item 24 says in as many words that
/// `Edit` and `Bash` *"were never tried"*. Every other name is therefore refused by the adapter,
/// naming the tool and the harness, rather than mapped to a guess: a guessed name that the CLI
/// silently ignores is the §12 accept-and-ignore shape with marion on the producing end, and this
/// axis exists precisely to end one instance of it.
pub const TOOL_WRITE: &str = "write";

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
    /// §3.1's `tools` key: the **built-in** tools this type's nodes may use, in *marion's*
    /// vocabulary ([`TOOL_WRITE`]), which each adapter maps to its harness's own spelling.
    /// An **allowlist, never a denylist** (§3.1), and never a route to marion's own MCP verbs —
    /// those ride the permission axis and a child cannot grant itself one by naming it here.
    ///
    /// **Both of §3.1's axes follow from this one list**, and that is the field's whole reason for
    /// existing rather than a convenience. Availability alone is *necessary and not sufficient*
    /// (§11 item 24, measured): a Claude Code node handed `--tools "Write"` and nothing else sends
    /// the call to `--permission-prompt-tool stdio`, where marion has no answerer, and the child's
    /// `tool_result` is item 22's dead-end message instead of a write. Deriving availability and
    /// permission from one declaration is what makes them unable to disagree.
    ///
    /// **The default is empty, and empty is exactly the behaviour every node has had until now** —
    /// `--tools ""` on Claude Code, gemini's default approval mode, i.e. no built-in tool at all.
    /// **No orchestrator type states one.** A tool declared here widens what *every* node of that
    /// type may do, so it is stated only by the `-impl` types, which is the difference between
    /// closing item 24 and hardcoding a tool name to make a matrix green.
    ///
    /// **Two separate protections, and only the second guards the operator's repository.** The
    /// empty default is about not widening a type that already exists. Keeping the grant on
    /// separate implementer types is about *where the node runs*: `run_spawn` gives a child a git
    /// worktree, while `root::prepare` compiles a root with `cwd` set to the operator's own repo.
    /// Neither is sufficient alone — an operator can type `marion run claude-impl` — so
    /// `root::prepare` compiles **no** availability axis at all, whatever type it resolves. That is
    /// the invariant; this field's naming convention is only the signpost.
    pub tools: Vec<String>,
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
            // §3.1's documented default, and the one value that keeps every built-in compiling the
            // bytes it compiled before this field existed. See the field's doc comment.
            tools: Vec::new(),
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
        // **The two implementer types, and the only built-ins that declare a tool.**
        //
        // §11 item 24: a claude or gemini child was read-only by construction, so marion spawned it
        // to do work and gave it no route to any — and the contract it persisted was byte-identical
        // to one whose write had escaped its worktree. These close that, and they are *new names*
        // rather than a widening of `claude` and `gemini` for two reasons that are not the same
        // reason:
        //
        // 1. An existing type that grew a tool would widen every node anyone already runs under it.
        // 2. `claude` is the **root orchestrator** — its own description says so — and a root is
        //    compiled with `cwd` set to the operator's own repository rather than a worktree. A
        //    grant on that type is a write tool pointed at the user's tree.
        //
        // `codex-impl` is the precedent for both: the implementer flavour has always been a
        // separate name beside the orchestrator, and this is what that name was always for.
        //
        // The second reason is *not* discharged by naming, since `marion run claude-impl` resolves
        // right here — `marion_supervisor::root::prepare` compiles no availability axis at all, and
        // that is what makes it an invariant. See the `tools` field's doc comment.
        "claude-impl" => Some(AgentType {
            tools: vec![TOOL_WRITE.into()],
            ..AgentType::defaults(
                "claude-impl",
                "Implements a well-specified change on Claude Code.",
                Harness::ClaudeCode,
            )
        }),
        "gemini-impl" => Some(AgentType {
            model: Some(GEMINI_DEFAULT_MODEL.into()),
            tools: vec![TOOL_WRITE.into()],
            ..AgentType::defaults(
                "gemini-impl",
                "Implements a well-specified change on the Gemini CLI.",
                Harness::Gemini,
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
    &[
        "claude",
        "claude-impl",
        "codex",
        "codex-impl",
        "gemini",
        "gemini-impl",
        "opencode",
    ]
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
            ("claude-impl", Harness::ClaudeCode),
            ("codex", Harness::Codex),
            ("codex-impl", Harness::Codex),
            ("gemini", Harness::Gemini),
            ("gemini-impl", Harness::Gemini),
            ("opencode", Harness::OpenCode),
        ] {
            assert_eq!(builtin(name).unwrap().harness, h, "{name}");
            assert!(builtin_names().contains(&name), "{name} must be listed");
        }
        assert_eq!(
            builtin_names().len(),
            7,
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
        assert_eq!(builtin("claude-impl").unwrap().model, None);
        assert_eq!(
            builtin("gemini").unwrap().model.as_deref(),
            Some(GEMINI_DEFAULT_MODEL)
        );
        assert_eq!(
            builtin("gemini-impl").unwrap().model.as_deref(),
            Some(GEMINI_DEFAULT_MODEL),
            "the -impl flavour launches on the same adapter, which refuses without a model"
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

    /// **Exactly the `-impl` types declare a tool, and the orchestrator types declare none.**
    ///
    /// Stated as an exhaustive partition rather than as two spot checks, so that a *new* built-in
    /// has to choose a side deliberately. Both directions are load-bearing and for different
    /// reasons (see the `tools` field's doc comment): a tool appearing on `claude` would widen
    /// every node anyone already runs under the orchestrator type *and* point a write tool at the
    /// operator's own repository through `marion run`, while a tool disappearing from
    /// `claude-impl` would put §11 item 24's gap back with the axis still nominally present.
    ///
    /// `codex-impl` and `opencode` are the honest asymmetry: their harnesses grant writes
    /// unconditionally already (`sandbox_mode = "workspace-write"`; opencode's own default tool
    /// list), so a declaration there would be a no-op dressed as a grant. The vocabulary describes
    /// what marion *compiles*, and on those two it compiles nothing.
    #[test]
    fn exactly_the_impl_types_that_need_a_grant_declare_one() {
        for name in builtin_names() {
            let declared = builtin(name).unwrap().tools;
            let expected: Vec<String> = match *name {
                "claude-impl" | "gemini-impl" => vec![TOOL_WRITE.into()],
                _ => vec![],
            };
            assert_eq!(
                declared, expected,
                "{name}: the orchestrator types must stay read-only and the -impl types must not \
                 lose the grant that closes item 24"
            );
        }
    }

    /// The `-impl` types are **additive**: the type an existing caller names is untouched.
    ///
    /// `run_spawn` defaults `spawn`'s `agent_type` to `codex-impl` and `marion run` takes a name
    /// from argv, so every node marion launches today resolves one of these four. If any of them
    /// grew a tool, this axis would have widened production rather than opened a route.
    #[test]
    fn the_types_that_already_existed_compile_the_declaration_they_always_had() {
        for name in ["claude", "codex", "codex-impl", "gemini", "opencode"] {
            assert!(
                builtin(name).unwrap().tools.is_empty(),
                "{name}: existed before the availability axis and must be unchanged by it"
            );
        }
    }

    /// An `-impl` type is the same type as its orchestrator in every respect but the grant.
    ///
    /// Not cosmetic: if `claude-impl` drifted to another harness or another set of gates it would
    /// stop being "the implementer flavour" and become a second definition of claude, which is the
    /// thing `codex`/`codex-impl` resolving to one definition exists to prevent.
    #[test]
    fn an_impl_type_differs_from_its_orchestrator_only_in_the_grant() {
        for (orchestrator, implementer) in [("claude", "claude-impl"), ("gemini", "gemini-impl")] {
            let o = builtin(orchestrator).unwrap();
            let i = builtin(implementer).unwrap();
            assert_eq!(i.harness, o.harness, "{implementer}");
            assert_eq!(i.model, o.model, "{implementer}");
            assert_eq!(i.scope_ceiling, o.scope_ceiling, "{implementer}");
            assert_eq!(i.timeout, o.timeout, "{implementer}");
            assert_eq!(i.max_depth, o.max_depth, "{implementer}");
            assert_eq!(
                i.max_concurrent_children, o.max_concurrent_children,
                "{implementer}"
            );
            assert_ne!(
                i.tools, o.tools,
                "{implementer}: the grant is the difference"
            );
            assert!(is_valid_name(&i.name), "{implementer}");
        }
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
