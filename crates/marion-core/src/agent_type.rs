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

/// The `copilot` built-in's default model.
///
/// Copilot's BYOK path refuses to start without an explicit model (1.0.83: `BYOK providers require
/// an explicit model`, exit 1 before any request), and marion's canned endpoint ignores the name.
/// So, as with [`OPENCODE_DEFAULT_MODEL`], this names marion's own plumbing rather than a vendor's
/// catalogue — and, as there, the adapter refuses it by name under `--live`, where the string would
/// go to GitHub's model routing and name nothing. An operator running live names a real Copilot
/// model through `spawn`'s `model` / `marion run -m`.
pub const COPILOT_DEFAULT_MODEL: &str = "marion-canned";

/// The first entry in §3.1's `tools:` vocabulary: *may create or overwrite a file*.
///
/// **A vocabulary of two words, and the second arrived the way the first did.** §3.1's example line
/// reads `tools: [read, edit, bash]`, and those three are the vocabulary's *shape*, not a catalogue
/// marion has earned. `write` was the first verb whose harness-native mapping was **measured** on
/// the two harnesses that were blocked (§11 item 24: `claude` 2.1.222 declares `Write` under
/// `--tools "Write"`; `gemini` 0.53.0 declares `write_file` under `--approval-mode auto_edit`);
/// [`TOOL_READ`] is the second, measured on all four in `tests/fixtures/s14/`. `edit` and `bash`
/// are still refused, because item 24 says in as many words that `Edit` and `Bash` *"were never
/// tried"* and s14 declared `Bash` once only to settle a separator. Every unmapped name is refused
/// by the adapter, naming the tool and the harness, rather than mapped to a guess: a guessed name
/// that the CLI silently ignores is the §12 accept-and-ignore shape with marion on the producing
/// end, and this axis exists precisely to end one instance of it.
pub const TOOL_WRITE: &str = "write";

/// The second entry in §3.1's `tools:` vocabulary: *may read the contents of a file*.
///
/// **Measured before it was named**, on all four installed harnesses, against the workspace's own
/// canned provider at a total spend of $0.00 — `tests/fixtures/s14/README.md` carries the argv, the
/// verbatim declarations off the wire, and the probes. The mapping each adapter compiles:
///
/// | harness | `read` maps to | what marion compiles | measured |
/// |---|---|---|---|
/// | claude 2.1.222 | `Read` | `--tools Read` **and** `--allowedTools Read` | a real grant: under marion's `--tools ""` there is no `Read` at all |
/// | gemini 0.53.0 | `read_file` | nothing | a no-op: `read_file` is in `functionDeclarations` by default |
/// | opencode 1.17.3 | `read` | nothing | a no-op: `read` is in the default tool list |
/// | codex 0.146.0 | **nothing — there is no read tool** | — | refused by name; reading is `exec_command`, i.e. the shell |
///
/// **codex is refused rather than mapped, and that is the design decision this constant carries.**
/// The tempting arm is the one [`TOOL_WRITE`] uses on that harness — *satisfied rather than newly
/// granted*, `sandbox:workspace-write`. It does not transfer. `write` maps to a **measured
/// correspondence**, `apply_patch` gated by a sandbox mode marion actually compiles; reading maps
/// to the **shell**, which also writes, execs and reaches the network. Answering `read` with it
/// would let a reader of `tools: [read]` believe a codex node was read-only when it is not — the
/// field-name-lies class this codebase refuses elsewhere (`working_tree_delta`, `scope_enforced`).
/// So `CodexAdapter::tool_name` grows no arm and the declaration aborts the launch by name.
///
/// **Why refusal and not a recorded `Unavailable { harness, verb }` in the compiled spec.** That
/// third option is accept-and-ignore wearing a better name *in this codebase*, because nothing
/// would read it: `marion doctor` does not exist, and §3.1's `tools:` is an allowlist whose whole
/// semantic is *"the node may do this"* — recording "may not, actually" inside it inverts the
/// field. A record no reader opens is a silent drop with extra steps. Refusal is also the
/// reversible direction (§11 item 23, `77557e3`): it can be downgraded to a visible record the day
/// a reader exists, whereas a caller taught that an empty tools axis is normal cannot be untaught.
/// s14 is what makes that concrete rather than stylistic — **claude, gemini and opencode each
/// silently ignore an unknown tool name**, on three *different* configuration surfaces
/// (`--tools`, `--allowed-tools`, `OPENCODE_PERMISSION`). Claude's is the sharpest recording:
/// `--tools NotATool` → `[]`, exit 0, empty stderr, and a `system/init` frame that agrees. So a
/// guessed or unsatisfiable mapping produces a run that looks completely healthy and simply has no
/// tool.
///
/// **Codex is not a fourth data point on that axis.** It has no per-tool flag at all — `--tools` is
/// a hard argv error — so it errors because the flag does not exist, not because it validates
/// names. Reading it as "one of the four rejects bad names" would suggest the axis is safe
/// somewhere, and it is safe nowhere.
pub const TOOL_READ: &str = "read";

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
    /// **Which ACP agent**, on the one harness where naming the harness is not naming a program.
    ///
    /// `Harness::Acp` is a *protocol*: one adapter serves many agents, they run different argv, and
    /// they spell marion's own verbs three different ways (S21: `marion_report`; S22:
    /// `mcp__marion__report` and `mcp.marion.report`). So `harness` alone cannot select behaviour
    /// there the way it does on the other four, and this is the field that finishes the selection.
    ///
    /// `None` on every non-ACP type, and **`None` on an ACP type is a refusal rather than a
    /// default** (§6.4: marion may not choose an agent for the operator). s14 measured what a
    /// default would buy: an unknown tool name is silently ignored, so an ACP node launched at the
    /// wrong agent's spelling ends `end_turn` having called nothing, and marion records a healthy
    /// run that delegated nothing.
    pub acp_agent: Option<String>,
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
    /// **This list now reaches a root too, and what guards the operator's repository is a record
    /// rather than a refusal.** `run_spawn` gives a child a git worktree marion made and later
    /// removes; `root::prepare` compiles a root with `cwd` set to the operator's own checkout. That
    /// asymmetry used to be answered by `root::prepare` compiling **no** availability axis at all,
    /// whatever type it resolved — an invariant, with the `-impl` naming convention as its signpost.
    ///
    /// That invariant made two arguments and only one of them survived scrutiny. **Containment**
    /// was overruled deliberately: a root is the operator's own node, started by their own hand and
    /// watched live, and nothing about it was ever contained. **Audit** was the real one — a root
    /// that wrote produced the same empty `changed_paths` as a child whose write escaped its
    /// worktree (§11 item 24, and `8a69f22`) — and it is now answered by a mechanism instead of by
    /// an absence: `root::RootChangeBase` takes the operator's working tree as a git tree object at
    /// launch and at exit, and `root::availability_axis` is the seam that joins the two. **A root's
    /// axis is this list, and a non-empty one is reachable only through the arm holding a base
    /// point** — otherwise the launch is refused as `root::RootError::NoChangeRecord`, or the
    /// operator declined the record in as many words with `marion run --no-change-record` and gets
    /// no tools. *No audit, no grant.* The convention is still only a signpost; the gate is the
    /// guarantee, and it holds for an agent type added later whose name carries no warning at all.
    ///
    /// This comment, `root::availability_axis` and that gate move together or they disagree.
    pub tools: Vec<String>,
    /// Ceiling only. `spawn` may narrow it and never widen it (§5.4).
    pub scope_ceiling: Vec<Glob>,
    pub timeout: Duration,
    pub max_depth: u32,
    pub max_concurrent_children: u32,
}

impl AgentType {
    /// **Does a node of this type have write tools?** — §6.6's *"at most one node with write tools
    /// per cwd"*, made a question the code can ask.
    ///
    /// **Two terms, and reading only the first is the bug this function was written with.**
    ///
    /// `tools:` is a *grant list* — what marion must positively enable that the harness would not
    /// do on its own — so it answers "did the operator ask for write?" and not "can this node
    /// write?". On codex and opencode those come apart completely:
    /// [`Harness::writes_without_a_declaration`] carries the measurement, and the short form is
    /// that `codex-impl` — marion's own canonical implementer, the type every worked example
    /// spawns, the one that applies patches — declares **no tools at all** and writes freely under
    /// `sandbox_mode = "workspace-write"`. A `tools:`-only predicate therefore said "does not
    /// write" about the single most common writing node marion runs, which would have left §6.6's
    /// occupancy guard compiling, passing its own tests, and protecting nothing in the case it
    /// exists for.
    ///
    /// Not read off `scope_ceiling` either, and that is a different mistake worth naming. A scope
    /// is a *ceiling on where* a write would be allowed to land; every type has one, and an
    /// orchestrator's default `["**"]` does not make it able to write anything. Keying occupancy on
    /// the scope would put every read-only node in §6.6's table and refuse a second reader from a
    /// directory no writer is in, which is a refusal §6.6 does not ask for.
    ///
    /// [`TOOL_WRITE`] is the whole of the declared write vocabulary today. `edit` and `bash` are
    /// refused by every adapter and are not in this list because they are not mappable yet (§11
    /// item 24); when either is added it must be added here too, and that is a property of this
    /// being one predicate rather than a `contains` spelled out at the call site.
    pub fn writes_files(&self) -> bool {
        self.harness.writes_without_a_declaration() || self.tools.iter().any(|t| t == TOOL_WRITE)
    }

    /// A type carrying every §3.1 default, so a built-in only states what it changes.
    fn defaults(name: &str, description: &str, harness: Harness) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            harness,
            // Stated by the types that need one; see the field's doc comment for why the default
            // is an absence rather than a guess.
            model: None,
            // Stated only by the `acp` types, and meaningless on the other four. See the field.
            acp_agent: None,
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
        // right here. What discharges it is `marion_supervisor::root::availability_axis`: a root
        // gets this list only over a repository whose working tree marion is recording, and is
        // refused by name otherwise. See the `tools` field's doc comment.
        //
        // Both declare `read` beside `write` because a node that may create a file and may not
        // open one is §11 item 24 half-closed — measured in `tests/fixtures/s14/`, where claude's
        // `Read` is absent under marion's `--tools ""` and present under `--tools Read`.
        "claude-impl" => Some(AgentType {
            tools: vec![TOOL_READ.into(), TOOL_WRITE.into()],
            ..AgentType::defaults(
                "claude-impl",
                "Implements a well-specified change on Claude Code.",
                Harness::ClaudeCode,
            )
        }),
        "gemini-impl" => Some(AgentType {
            model: Some(GEMINI_DEFAULT_MODEL.into()),
            tools: vec![TOOL_READ.into(), TOOL_WRITE.into()],
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
        // The fifth binary, on the same footing as `gemini` and `opencode`: the harness's own name,
        // the defaults, and a model because its adapter refuses to launch canned without one.
        "copilot" => Some(AgentType {
            model: Some(COPILOT_DEFAULT_MODEL.into()),
            ..AgentType::defaults(
                "copilot",
                "Implements a well-specified change on the GitHub Copilot CLI.",
                Harness::Copilot,
            )
        }),
        // The implementer flavour, for the same reason `claude-impl` and `gemini-impl` exist: the
        // adapter withholds every built-in it is not told to declare (`--available-tools`), so a
        // `copilot` child has no route to a file at all, and the grant has to live on a type.
        "copilot-impl" => Some(AgentType {
            model: Some(COPILOT_DEFAULT_MODEL.into()),
            tools: vec![TOOL_READ.into(), TOOL_WRITE.into()],
            ..AgentType::defaults(
                "copilot-impl",
                "Implements a well-specified change on the GitHub Copilot CLI.",
                Harness::Copilot,
            )
        }),
        // **One built-in per ACP agent, and no built-in named `acp`.** The other four harnesses
        // get a type named after the harness because there the harness *is* the program. Here it
        // is not: a type named `acp` would have to pick an agent, and §6.4 says marion may not.
        //
        // `opencode acp` is the one agent that is both measured to a tool call (S21) and has a
        // recipe for marion's canned provider, which is what lets an ACP node run in the default
        // suite at $0.00. The two ACP Registry shims are measured too (S22) but reach a provider
        // only through the operator's own vendor login, so a built-in for either would be a type
        // that cannot run without real spend; they stay reachable through `acp::AGENTS` — the
        // doctor probes them — and earn a built-in when a canned recipe for them is measured.
        "acp-opencode" => Some(AgentType {
            model: Some(OPENCODE_DEFAULT_MODEL.into()),
            acp_agent: Some("opencode".into()),
            ..AgentType::defaults(
                "acp-opencode",
                "Implements a well-specified change on opencode over the Agent Client Protocol.",
                Harness::Acp,
            )
        }),
        _ => None,
    }
}

/// Every built-in name, aliases included — what `marion doctor` would list.
pub fn builtin_names() -> &'static [&'static str] {
    &[
        "acp-opencode",
        "claude",
        "claude-impl",
        "codex",
        "codex-impl",
        "copilot",
        "copilot-impl",
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
            ("acp-opencode", Harness::Acp),
            ("claude", Harness::ClaudeCode),
            ("claude-impl", Harness::ClaudeCode),
            ("codex", Harness::Codex),
            ("codex-impl", Harness::Codex),
            ("copilot", Harness::Copilot),
            ("copilot-impl", Harness::Copilot),
            ("gemini", Harness::Gemini),
            ("gemini-impl", Harness::Gemini),
            ("opencode", Harness::OpenCode),
        ] {
            assert_eq!(builtin(name).unwrap().harness, h, "{name}");
            assert!(builtin_names().contains(&name), "{name} must be listed");
        }
        assert_eq!(
            builtin_names().len(),
            10,
            "a new built-in must be listed here too, or `marion doctor` would not name it"
        );
    }

    /// **A type on the ACP row names its agent, and a type on any other row names none.**
    ///
    /// `harness` selects an adapter on four of the five rows and does not finish the job on the
    /// fifth: `acp` is a protocol whose agents run different argv and spell marion's verbs three
    /// different ways. A built-in that named `Harness::Acp` and left `acp_agent` empty would be
    /// refused at launch — which is the correct behaviour and a useless type — and one that
    /// carried an agent id on a non-ACP row would be carrying a value nothing reads, i.e. a
    /// setting an operator could change with no effect.
    ///
    /// Stated over `builtin_names()` rather than over today's list, so a type added later has to
    /// land on one side or the other. The id itself is checked against
    /// `marion_harness::acp::AGENTS` one crate up, because this crate is below the registry.
    #[test]
    fn exactly_the_acp_builtins_name_an_acp_agent() {
        let mut acp = 0;
        for name in builtin_names() {
            let t = builtin(name).unwrap_or_else(|| panic!("{name} is listed and must resolve"));
            match t.harness {
                Harness::Acp => {
                    assert!(
                        t.acp_agent.is_some(),
                        "{name} names the ACP protocol and no agent, so it can never launch"
                    );
                    acp += 1;
                }
                other => assert_eq!(
                    t.acp_agent, None,
                    "{name} is on the {other} row, where an ACP agent id is read by nothing"
                ),
            }
        }
        assert!(acp > 0, "both sides of this must be exercised");
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
        assert_eq!(
            builtin("copilot").unwrap().model.as_deref(),
            Some(COPILOT_DEFAULT_MODEL),
            "BYOK refuses to start without a model, so the canned default must carry one"
        );
        assert_eq!(
            builtin("copilot-impl").unwrap().model.as_deref(),
            Some(COPILOT_DEFAULT_MODEL),
            "the -impl flavour launches on the same adapter, which refuses canned without a model"
        );
        assert_eq!(
            builtin("copilot-impl").unwrap().tools,
            vec![TOOL_READ.to_string(), TOOL_WRITE.to_string()],
            "the implementer flavour declares both, as claude-impl and gemini-impl do (§11 item 24)"
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
    /// what marion *compiles*, and on those two it compiles nothing. `copilot-impl` sits with the
    /// first pair: its adapter withholds every built-in it is not told to declare.
    #[test]
    fn exactly_the_impl_types_that_need_a_grant_declare_one() {
        for name in builtin_names() {
            let declared = builtin(name).unwrap().tools;
            let expected: Vec<String> = match *name {
                "claude-impl" | "gemini-impl" | "copilot-impl" => {
                    vec![TOOL_READ.into(), TOOL_WRITE.into()]
                }
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

    /// **`read` is declared beside `write`, not instead of it, and the pairing is the point.**
    ///
    /// A claude child under `--tools "Write"` and nothing else could create a file and could not
    /// open one — §11 item 24 half-closed. `tests/fixtures/s14/README.md` measures the missing
    /// half: `Read` is absent under marion's `--tools ""` and present under `--tools Read`, on the
    /// 2.1.222 the machine actually has. The order is stated too, because it is what
    /// `--tools Read,Write` compiles to and s14 is also what paid off the comma-separator debt
    /// (`--tools "Read,Bash"` declares both).
    ///
    /// **codex-impl and opencode declare neither, and that is not an oversight.** codex has no read
    /// tool at all (s14: reading is `exec_command`), so a declaration there would be refused by its
    /// adapter and `marion run codex-impl` would stop launching. See [`TOOL_READ`].
    #[test]
    fn the_impl_types_can_read_what_they_write() {
        for name in ["claude-impl", "gemini-impl"] {
            let t = builtin(name).unwrap();
            assert_eq!(
                t.tools,
                vec![TOOL_READ.to_string(), TOOL_WRITE.to_string()],
                "{name}: a node that may write and may not read is item 24 half-closed"
            );
        }
        for name in ["codex-impl", "opencode"] {
            assert!(
                !builtin(name)
                    .unwrap()
                    .tools
                    .contains(&TOOL_READ.to_string()),
                "{name}: codex has no read tool, so declaring one would refuse the launch"
            );
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
