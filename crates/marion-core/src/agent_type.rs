//! The minimum honest agent-type registry for M1 (§3.1 key table, §6.1 step 2, §9).
//!
//! §3.1's full key set is a file format with discovery and precedence rules; none of that is M1.
//! What M1 genuinely needs is the handful of per-type values that other §-rules already read:
//! the writable-scope ceiling (§5.4), the node's timeout bound (§9), and the two `spawn` gates
//! (§6.1 step 2). So this is a plain lookup over two built-in types, not a loader — a loader with
//! no file format behind it would be scaffolding pretending to be a feature.
//!
//! Everything here is pure data plus one predicate; nothing reads a file. [`AgentTypes::parse`] is
//! the one loader, and it takes the file's **text**: the supervisor opens `.marion/agents.toml`
//! and hands the bytes down, so this crate stays free of I/O and the format stays testable as a
//! string.

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

/// The `goose` built-in's default model.
///
/// goose's `openai` provider names its model from `GOOSE_MODEL` and nothing else, and marion's
/// canned endpoint ignores the name — so, as with [`COPILOT_DEFAULT_MODEL`], this names marion's
/// own plumbing, and the adapter refuses it by name under `--live`, where the string would go to a
/// real OpenAI endpoint and name nothing.
pub const GOOSE_DEFAULT_MODEL: &str = "marion-canned";

/// The `cline` built-in's default model.
///
/// cline's provider is a `providers.json` document whose `model` field is mandatory, and marion's
/// canned endpoint ignores the name — so, as with [`COPILOT_DEFAULT_MODEL`], this names marion's
/// own plumbing, and the adapter refuses it by name under `--live`, where a real model rides `-m`
/// (measured to win over the operator's `providers.json`, S27) and none at all leaves theirs.
pub const CLINE_DEFAULT_MODEL: &str = "marion-canned";

/// The `qwen` built-in's default model.
///
/// qwen's OpenAI provider names its model from `OPENAI_MODEL` and marion's canned endpoint ignores
/// the name — so this names marion's own plumbing, as [`COPILOT_DEFAULT_MODEL`] does, and the
/// adapter refuses it by name under `--live`.
pub const QWEN_DEFAULT_MODEL: &str = "marion-canned";

/// The default models that name marion's own canned plumbing rather than a vendor's model, which
/// [`AgentType::default_model`] drops on a live run.
const CANNED_PLUMBING_MODELS: &[&str] = &[
    OPENCODE_DEFAULT_MODEL,
    COPILOT_DEFAULT_MODEL,
    GOOSE_DEFAULT_MODEL,
    CLINE_DEFAULT_MODEL,
    QWEN_DEFAULT_MODEL,
    PI_DEFAULT_MODEL,
];

/// The `agy` built-ins' default model: a real slug, because agy has **no canned route** — it runs
/// only on the operator's own login (the adapter refuses a canned launch by name) — and the
/// cheapest one 1.2.8 lists (`agy models`), so a default node spends the least of the operator's
/// consumer quota (s32).
pub const AGY_DEFAULT_MODEL: &str = "gemini-3.6-flash-low";

/// The `pi` built-ins' default model.
///
/// pi's canned provider is a `models.json` document whose model list is mandatory, and marion's
/// canned endpoint ignores the name — so this names marion's own plumbing, as
/// [`COPILOT_DEFAULT_MODEL`] does, and the adapter refuses it by name under `--live`, where a real
/// model rides `--model` and none at all leaves the operator's own default.
pub const PI_DEFAULT_MODEL: &str = "marion-canned";

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
    /// type may do, so it is stated only by the implementer types (the plain harness names), which
    /// is the difference between closing item 24 and hardcoding a tool name to make a matrix green.
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
    /// Text the supervisor puts in front of every prompt a node of this type is given, once, at
    /// spawn — the operator's standing instruction for the type (`.marion/agents.toml`'s
    /// `prompt_prefix`). `None` on every built-in: the built-ins describe a harness and a grant,
    /// and marion does not put words in an operator's prompt on its own initiative. The contract
    /// records the prompt the node actually saw, prefix included.
    ///
    /// The supervisor joins it to the prompt with a single newline unless it already ends in
    /// whitespace, in which case it is joined exactly as written: end it in `"\n\n"` for a blank
    /// line, end it on a letter and marion supplies the line break.
    pub prompt_prefix: Option<String>,
    /// The ACP session mode a node of this type runs in (`.marion/agents.toml`'s `approval_mode`):
    /// the agent's own approval behaviour, chosen by its id as the agent advertises it —
    /// claude-agent-acp's `acceptEdits` or `bypassPermissions`, codex-acp's `agent-full-access`,
    /// copilot's `https://agentclientprotocol.com/protocol/session-modes#autopilot`. marion sets it
    /// over the protocol after `session/new` and refuses the run by name where the agent does not
    /// offer it. `None` — every built-in, and the default — leaves the agent in its own default
    /// mode. Only an ACP type may state one: the other harnesses have no session to set it in.
    pub approval_mode: Option<String>,
    /// The provider a node of this type talks to in endpoint mode (`.marion/agents.toml`'s
    /// `provider`), by `marion_core::provider` registry id. `None` — every built-in — is no
    /// provider: the node runs canned or on the operator's own login. A `<provider>:<model>`
    /// prefix on `model` names one too; this field is the explicit spelling.
    pub provider: Option<String>,
    /// The order a node of this type tries its provider's credentials in (`credentials =
    /// ["openrouter:work", "openrouter"]`), each a `marion_core::provider::CredentialId` of
    /// [`Self::provider`]. Empty — every built-in — defers to the user's own order.
    pub credentials: Vec<String>,
    /// The profiles a node of this type runs on (`.marion/agents.toml`'s `profile`, one name or a
    /// list): the first is the account the node uses, and the rest are the failover order for an
    /// **expired or refused login only** — never for a usage limit. Empty, the default, is the
    /// harness's own default login, or `profiles.toml`'s `[default]` for the harness.
    pub profiles: Vec<String>,
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

    /// The model a spawn of this type runs on when the request names none.
    ///
    /// On a live run (`canned == false`) a default that names marion's own canned plumbing is
    /// dropped: it exists so a canned node can launch, it names nothing on the operator's own
    /// account, and without it the harness uses the operator's own default model, as it uses their
    /// login. A real vendor id (gemini's) is kept either way.
    pub fn default_model(&self, canned: bool) -> Option<String> {
        match &self.model {
            Some(m) if !canned && CANNED_PLUMBING_MODELS.contains(&m.as_str()) => None,
            other => other.clone(),
        }
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
            prompt_prefix: None,
            approval_mode: None,
            provider: None,
            credentials: Vec::new(),
            profiles: Vec::new(),
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

/// One built-in agent type as **data**. Every arm of the old `match` differed only in these
/// fields, so they are a table rather than fifteen hand-written constructors: a new harness is a
/// row, and the resolution rule below is written once.
///
/// `canonical` is the name the supervisor writes into contracts. `aliases` are the other spellings
/// that resolve to the *same* definition, rather than to a second one that could drift.
struct Builtin {
    canonical: &'static str,
    aliases: &'static [&'static str],
    description: &'static str,
    harness: Harness,
    /// `None` where §3.1's "absence rather than a guess" default applies; see the field it fills.
    model: Option<&'static str>,
    /// The grant list — empty for every type that declares no tool. See [`AgentType::tools`].
    tools: &'static [&'static str],
    /// Stated only by the `acp` rows, and meaningless on the others.
    acp_agent: Option<&'static str>,
}

impl Builtin {
    /// §3.1 resolution: the canonical name, or any alias of it.
    fn matches(&self, name: &str) -> bool {
        self.canonical == name || self.aliases.contains(&name)
    }

    /// The row over [`AgentType::defaults`], so a row still states only what it changes.
    fn build(&self) -> AgentType {
        AgentType {
            model: self.model.map(str::to_string),
            acp_agent: self.acp_agent.map(str::to_string),
            tools: self.tools.iter().map(|t| (*t).to_string()).collect(),
            ..AgentType::defaults(self.canonical, self.description, self.harness)
        }
    }
}

/// The built-in types, by name. `None` is a spawn error (§6.1 step 2 resolves the type first).
///
/// **A plain harness name is that harness's full implementer, and `<harness>-impl` is an alias of
/// it.** One rule for every harness, because a model choosing a child reaches for the plain name:
/// when plain `claude` was a read-only orchestrator and plain `codex` an implementer, a live
/// opencode root spawned `claude` to edit a file twice, and the child could only fail. `-impl`
/// stays a spelling of the same row, so every script and prompt that names it keeps resolving, and
/// the name written into a contract is the plain one.
///
/// **The read-only flavour has a name that says so**: `<harness>-orchestrator`, on exactly the
/// harnesses where marion can withhold writes ([`Harness::writes_without_a_declaration`] is
/// false). Where the harness writes on its own (codex, opencode, cline) a "read-only" type would
/// describe a grant marion does not compile, so there is none.
///
/// What guards the operator's checkout from a root with a grant is
/// `marion_supervisor::root::availability_axis`: a root gets a grant list only over a repository
/// whose working tree marion is recording, and is refused by name otherwise. See the `tools`
/// field's doc comment.
///
/// Most implementers declare `read` beside `write`, because a node that may create a file and may
/// not open one is §11 item 24 half-closed — measured in `tests/fixtures/s14/`, where claude's
/// `Read` is absent under marion's `--tools ""` and present under `--tools Read`.
const BUILTINS: &[Builtin] = &[
    Builtin {
        canonical: "claude",
        aliases: &["claude-impl"],
        description: "Implementer on Claude Code: may read and write files.",
        harness: Harness::ClaudeCode,
        model: None,
        tools: &[TOOL_READ, TOOL_WRITE],
        acp_agent: None,
    },
    Builtin {
        canonical: "claude-orchestrator",
        aliases: &[],
        description: "Orchestrator on Claude Code: plans and delegates through marion; cannot \
                      write files.",
        harness: Harness::ClaudeCode,
        model: None,
        tools: &[],
        acp_agent: None,
    },
    // No `tools`: codex grants writes under its own `workspace-write` sandbox, and has no read
    // tool for `read` to map to (see [`TOOL_READ`]).
    Builtin {
        canonical: "codex",
        aliases: &["codex-impl"],
        description: "Implementer on Codex: may read, write and run commands in its sandbox.",
        harness: Harness::Codex,
        model: None,
        tools: &[],
        acp_agent: None,
    },
    Builtin {
        canonical: "gemini",
        aliases: &["gemini-impl"],
        description: "Implementer on the Gemini CLI: may read and write files.",
        harness: Harness::Gemini,
        model: Some(GEMINI_DEFAULT_MODEL),
        tools: &[TOOL_READ, TOOL_WRITE],
        acp_agent: None,
    },
    Builtin {
        canonical: "gemini-orchestrator",
        aliases: &[],
        description: "Orchestrator on the Gemini CLI: plans and delegates through marion; cannot \
                      write files.",
        harness: Harness::Gemini,
        model: Some(GEMINI_DEFAULT_MODEL),
        tools: &[],
        acp_agent: None,
    },
    // No `tools`: opencode's default tool list already grants reads and writes.
    Builtin {
        canonical: "opencode",
        aliases: &["opencode-impl"],
        description: "Implementer on opencode: all of opencode's own tools.",
        harness: Harness::OpenCode,
        model: Some(OPENCODE_DEFAULT_MODEL),
        tools: &[],
        acp_agent: None,
    },
    // The adapter withholds every built-in it is not told to declare (`--available-tools`), so
    // the grant has to live on the type.
    Builtin {
        canonical: "copilot",
        aliases: &["copilot-impl"],
        description: "Implementer on the GitHub Copilot CLI: may read and write files.",
        harness: Harness::Copilot,
        model: Some(COPILOT_DEFAULT_MODEL),
        tools: &[TOOL_READ, TOOL_WRITE],
        acp_agent: None,
    },
    Builtin {
        canonical: "copilot-orchestrator",
        aliases: &[],
        description: "Orchestrator on the GitHub Copilot CLI: plans and delegates through marion; \
                      cannot write files.",
        harness: Harness::Copilot,
        model: Some(COPILOT_DEFAULT_MODEL),
        tools: &[],
        acp_agent: None,
    },
    // `write` compiles `--with-builtin developer`; `read` is deliberately absent — the developer
    // extension has no read-only tool, and answering `read` with an extension that also carries
    // `shell` and `write` would mislabel the node (S26).
    Builtin {
        canonical: "goose",
        aliases: &["goose-impl"],
        description: "Implementer on goose: may write files and run shell commands.",
        harness: Harness::Goose,
        model: Some(GOOSE_DEFAULT_MODEL),
        tools: &[TOOL_WRITE],
        acp_agent: None,
    },
    Builtin {
        canonical: "goose-orchestrator",
        aliases: &[],
        description: "Orchestrator on goose: plans and delegates through marion; cannot write \
                      files.",
        harness: Harness::Goose,
        model: Some(GOOSE_DEFAULT_MODEL),
        tools: &[],
        acp_agent: None,
    },
    // On opencode's footing: cline grants its 26 built-ins unconditionally, so a declaration
    // would name a grant marion does not compile (S27 item 12). A model because `providers.json`
    // needs one.
    Builtin {
        canonical: "cline",
        aliases: &["cline-impl"],
        description: "Implementer on the Cline CLI: all of cline's own tools.",
        harness: Harness::Cline,
        model: Some(CLINE_DEFAULT_MODEL),
        tools: &[],
        acp_agent: None,
    },
    // The adapter offers only what `--core-tools` names, so the grant has to live on the type.
    Builtin {
        canonical: "qwen",
        aliases: &["qwen-impl"],
        description: "Implementer on Qwen Code: may read and write files.",
        harness: Harness::Qwen,
        model: Some(QWEN_DEFAULT_MODEL),
        tools: &[TOOL_READ, TOOL_WRITE],
        acp_agent: None,
    },
    Builtin {
        canonical: "qwen-orchestrator",
        aliases: &[],
        description: "Orchestrator on Qwen Code: plans and delegates through marion; cannot write \
                      files.",
        harness: Harness::Qwen,
        model: Some(QWEN_DEFAULT_MODEL),
        tools: &[],
        acp_agent: None,
    },
    // The ninth binary, run only on the operator's own login: agy has no canned provider route.
    // The default `request-review` mode auto-denies a headless write, and a `write` declaration
    // is what compiles `--mode accept-edits` (s32), so the grant lives on the type.
    Builtin {
        canonical: "agy",
        aliases: &["agy-impl"],
        description: "Implementer on Google's Antigravity CLI: may read and write files.",
        harness: Harness::Antigravity,
        model: Some(AGY_DEFAULT_MODEL),
        tools: &[TOOL_READ, TOOL_WRITE],
        acp_agent: None,
    },
    Builtin {
        canonical: "agy-orchestrator",
        aliases: &[],
        description: "Orchestrator on Google's Antigravity CLI: plans and delegates through \
                      marion; cannot write files.",
        harness: Harness::Antigravity,
        model: Some(AGY_DEFAULT_MODEL),
        tools: &[],
        acp_agent: None,
    },
    // The tenth binary. The adapter offers only what `--tools` names, extension tools included,
    // so a grant has to live on a type (S34).
    Builtin {
        canonical: "pi",
        aliases: &[],
        description: "Implementer on pi: may read and write files.",
        harness: Harness::Pi,
        model: Some(PI_DEFAULT_MODEL),
        tools: &[TOOL_READ, TOOL_WRITE],
        acp_agent: None,
    },
    Builtin {
        canonical: "pi-orchestrator",
        aliases: &[],
        description: "Orchestrator on pi: plans and delegates through marion; cannot write files.",
        harness: Harness::Pi,
        model: Some(PI_DEFAULT_MODEL),
        tools: &[],
        acp_agent: None,
    },
    // **One built-in per ACP agent, and no built-in named `acp`.** The other harnesses get a type
    // named after the harness because there the harness *is* the program. Here it is not: a type
    // named `acp` would have to pick an agent, and §6.4 says marion may not.
    //
    // The rule is mechanical: every `marion_harness::acp::AGENTS` row whose `session/new` opened a
    // session (S33) is `acp-<row id>`, bound to that row, and a row stopped at an account wall has
    // none. `marion_harness`'s `every_opened_row_is_a_builtin_type_and_every_acp_builtin_is_a_row`
    // holds the two lists together.
    //
    // `opencode acp` is the one agent that is both measured to a tool call (S21) and has a recipe
    // for marion's canned provider, which is what lets an ACP node run in the default suite at
    // $0.00 — so it is also the one that states a model. Every other ACP type runs on the
    // operator's own login or provider configuration for that agent (marion inherits it, never
    // chooses it), in the model that configuration selects unless `spawn` names one.
    Builtin {
        canonical: "acp-opencode",
        aliases: &[],
        description: "Implementer on opencode over the Agent Client Protocol: all of opencode's \
                      own tools.",
        harness: Harness::Acp,
        model: Some(OPENCODE_DEFAULT_MODEL),
        tools: &[],
        acp_agent: Some("opencode"),
    },
    Builtin {
        canonical: "acp-claude-acp",
        aliases: &[],
        description: "Implements a well-specified change on Claude Code through the claude-agent-acp shim.",
        harness: Harness::Acp,
        model: None,
        tools: &[],
        acp_agent: Some("claude-acp"),
    },
    Builtin {
        canonical: "acp-codex-acp",
        aliases: &[],
        description: "Implements a well-specified change on Codex through the codex-acp shim.",
        harness: Harness::Acp,
        model: None,
        tools: &[],
        acp_agent: Some("codex-acp"),
    },
    Builtin {
        canonical: "acp-copilot",
        aliases: &[],
        description: "Implements a well-specified change on the GitHub Copilot CLI over the Agent Client Protocol.",
        harness: Harness::Acp,
        model: None,
        tools: &[],
        acp_agent: Some("copilot"),
    },
    Builtin {
        canonical: "acp-kilo",
        aliases: &[],
        description: "Implements a well-specified change on Kilo over the Agent Client Protocol.",
        harness: Harness::Acp,
        model: None,
        tools: &[],
        acp_agent: Some("kilo"),
    },
    Builtin {
        canonical: "acp-qwen",
        aliases: &[],
        description: "Implements a well-specified change on Qwen Code over the Agent Client Protocol.",
        harness: Harness::Acp,
        model: None,
        tools: &[],
        acp_agent: Some("qwen"),
    },
    Builtin {
        canonical: "acp-goose",
        aliases: &[],
        description: "Implements a well-specified change on goose over the Agent Client Protocol.",
        harness: Harness::Acp,
        model: None,
        tools: &[],
        acp_agent: Some("goose"),
    },
    Builtin {
        canonical: "acp-fast-agent",
        aliases: &[],
        description: "Implements a well-specified change on fast-agent over the Agent Client Protocol.",
        harness: Harness::Acp,
        model: None,
        tools: &[],
        acp_agent: Some("fast-agent"),
    },
    Builtin {
        canonical: "acp-vibe",
        aliases: &[],
        description: "Implements a well-specified change on Mistral Vibe over the Agent Client Protocol.",
        harness: Harness::Acp,
        model: None,
        tools: &[],
        acp_agent: Some("vibe"),
    },
    Builtin {
        canonical: "acp-vtcode",
        aliases: &[],
        description: "Implements a well-specified change on VT Code over the Agent Client Protocol.",
        harness: Harness::Acp,
        model: None,
        tools: &[],
        acp_agent: Some("vtcode"),
    },
];

/// A built-in by name, or the one open-ended family: any ACP agent, named by its command.
///
/// The table above is closed data; [`acp_command`] is the open half, and stays code because it
/// parses a command line rather than looking a name up.
pub fn builtin(name: &str) -> Option<AgentType> {
    match BUILTINS.iter().find(|b| b.matches(name)) {
        Some(b) => Some(b.build()),
        None => acp_command(name),
    }
}

/// The prefix that makes an agent-type string an ACP **command** rather than a name:
/// `acp:<command> [args…]`, e.g. `acp:copilot --acp`, `acp:goose acp`, `acp:npx -y some-acp-agent`.
///
/// This is the baseline §9's M5 rests on — *any* agent that speaks ACP over stdio runs as a marion
/// node with no row anywhere naming it. The protocol supplies everything a row would: the agent's
/// identity arrives in `initialize`, marion's bridge is declared in `session/new`'s `mcpServers`,
/// and the agent's spelling of marion's verbs is read off its own `tool_call` frames. The built-in
/// `acp-opencode` and the rows in `marion_harness::acp::AGENTS` are **refinements** over this path
/// (a measured spelling, a canned recipe, a known quirk), not prerequisites for it.
///
/// The prefix carries a colon and the tail carries spaces, so such a name deliberately fails
/// [`is_valid_name`]: it is a command line, and §3.1's name rule exists so that a *name* can never
/// become a surprising path or flag. Here the operator is stating the path and flags on purpose.
pub const ACP_COMMAND_PREFIX: &str = "acp:";

/// `acp:<command>` as a type: the ACP row, the whole tail as [`AgentType::acp_agent`]. `None` for
/// any other string, and for a prefix with nothing after it — a command with no program is not a
/// command, and refusing it here keeps the launch from ever building an empty argv.
///
/// The tail is not tokenised here: `marion_harness::acp` owns the split, so the one place that
/// turns the selector into argv is the one that also resolves a refinement row by id.
fn acp_command(name: &str) -> Option<AgentType> {
    let command = name.strip_prefix(ACP_COMMAND_PREFIX)?.trim();
    if command.is_empty() {
        return None;
    }
    Some(AgentType {
        acp_agent: Some(command.to_string()),
        ..AgentType::defaults(
            name,
            "Runs an ACP agent named by its command, over the Agent Client Protocol.",
            Harness::Acp,
        )
    })
}

/// Every built-in type's canonical name, once each and **no alias** — what the picker, the `spawn`
/// schema and `marion doctor` list. An alias still resolves through [`builtin`]; listing it beside
/// its type would print the same type twice under two names.
pub fn builtin_names() -> &'static [&'static str] {
    &[
        "acp-claude-acp",
        "acp-codex-acp",
        "acp-copilot",
        "acp-fast-agent",
        "acp-goose",
        "acp-kilo",
        "acp-opencode",
        "acp-qwen",
        "acp-vibe",
        "acp-vtcode",
        "claude",
        "claude-orchestrator",
        "codex",
        "gemini",
        "gemini-orchestrator",
        "opencode",
        "copilot",
        "copilot-orchestrator",
        "goose",
        "goose-orchestrator",
        "cline",
        "qwen",
        "qwen-orchestrator",
        "agy",
        "agy-orchestrator",
        "pi",
        "pi-orchestrator",
    ]
}

/// The agent types one working tree can spawn: the built-ins, plus the rows of that tree's
/// `.marion/agents.toml`, resolved through **one table and one path**.
///
/// §3.1 describes agent types as a file format; the built-ins above were the M1 stand-in for it,
/// and this is the format arriving without displacing them. A user row is an [`AgentType`] like any
/// other — same fields, same defaults, same adapters — and is refused where it would collide with
/// one, so `resolve("codex")` means the same thing in every tree. The supervisor loads one of these
/// per spawn from the tree the spawn is against (`marion_supervisor::run::agent_types`), so a
/// changed file is read at the next spawn and never cached across one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentTypes {
    user: Vec<AgentType>,
}

/// Why `.marion/agents.toml` was refused. **Every one is a load error and never a default** (§3.1):
/// a tree whose file is wrong spawns nothing until it is fixed, rather than spawning the built-ins
/// while silently ignoring the row the operator wrote.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentTypesError {
    /// TOML the parser refused, or a key no row has — `deny_unknown_fields`, so a misspelt
    /// `prompt_prefix` is an error and not a row that silently lost its prefix.
    #[error("{0}")]
    Syntax(String),
    #[error(
        "agent type name {0:?} is not a name: §3.1 names match ^[A-Za-z0-9][A-Za-z0-9_-]{{0,63}}$"
    )]
    InvalidName(String),
    #[error(
        "agent type {0:?} would shadow the built-in of that name (or its alias); pick another name"
    )]
    ShadowsBuiltin(String),
    #[error("agent type {0:?} is defined twice in the file")]
    Duplicate(String),
    #[error(
        "agent type {name:?} names harness {harness:?}; known harnesses are {known}, or \
         `acp:<command>` for any ACP agent",
        known = known_harnesses()
    )]
    UnknownHarness { name: String, harness: String },
    #[error(
        "agent type {name:?} declares tool {tool:?}; marion's tool vocabulary is {TOOL_READ}, {TOOL_WRITE}"
    )]
    UnknownTool { name: String, tool: String },
    #[error(
        "agent type {name:?} states approval_mode {mode:?} on harness {harness:?}; an approval \
         mode is an ACP session mode, set over the protocol, so only an `acp:<command>` type (or \
         harness = \"acp\") can have one"
    )]
    ApprovalModeOffAcp {
        name: String,
        harness: String,
        mode: String,
    },
    #[error(
        "agent type {name:?} names provider {provider:?}, which is not a provider id (lowercase \
         letters, digits and `-`); `marion login --list` shows the providers marion knows"
    )]
    InvalidProvider { name: String, provider: String },
    #[error(
        "agent type {name:?} lists credential {credential:?}, which is not a credential id of its \
         provider (`<provider>` or `<provider>:<label>`, and a row that lists credentials names \
         its provider)"
    )]
    InvalidCredential { name: String, credential: String },
    #[error(
        "agent type {name:?} names profile {profile:?}, which is not a name: profile names match \
         ^[A-Za-z0-9][A-Za-z0-9_-]{{0,63}}$"
    )]
    InvalidProfileName { name: String, profile: String },
}

/// `Harness::ALL`'s spellings, joined for [`AgentTypesError::UnknownHarness`]: Claude Code as
/// `claude`, the name a person types, with its wire spelling beside it.
fn known_harnesses() -> String {
    Harness::ALL
        .iter()
        .map(|h| match h {
            Harness::ClaudeCode => "claude (or claude-code)",
            other => other.as_str(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// One `[[agent]]` row as written. Every optional key is optional here and nowhere else: the row
/// becomes an [`AgentType`] over [`AgentType::defaults`], so a key the file omits is §3.1's default
/// and never this struct's.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FileRow {
    name: String,
    harness: String,
    description: String,
    model: Option<String>,
    tools: Option<Vec<String>>,
    prompt_prefix: Option<String>,
    approval_mode: Option<String>,
    provider: Option<String>,
    credentials: Option<Vec<String>>,
    profile: Option<ProfileKey>,
}

/// `profile = "work"` or `profile = ["work", "personal"]`.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ProfileKey {
    One(String),
    Many(Vec<String>),
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    agent: Vec<FileRow>,
}

impl AgentTypes {
    /// The table a tree with no `.marion/agents.toml` has.
    pub fn builtins_only() -> Self {
        Self::default()
    }

    /// The table a tree's `.marion/agents.toml` describes, or the first reason it cannot.
    pub fn parse(text: &str) -> Result<Self, AgentTypesError> {
        let file: File =
            toml::from_str(text).map_err(|e| AgentTypesError::Syntax(e.to_string()))?;
        let mut user: Vec<AgentType> = Vec::with_capacity(file.agent.len());
        for row in file.agent {
            let ty = row.into_type()?;
            if user.iter().any(|u| u.name == ty.name) {
                return Err(AgentTypesError::Duplicate(ty.name));
            }
            user.push(ty);
        }
        Ok(Self { user })
    }

    /// A type by name: the file's row, else a built-in, else the `acp:<command>` family.
    ///
    /// The file cannot shadow a built-in ([`AgentTypesError::ShadowsBuiltin`]), so the order here
    /// is a convenience and not a precedence rule.
    pub fn resolve(&self, name: &str) -> Option<AgentType> {
        self.user
            .iter()
            .find(|t| t.name == name)
            .cloned()
            .or_else(|| builtin(name))
    }

    /// Every name that resolves, for a picker or a refusal: the built-ins, then the file's rows in
    /// the order the operator wrote them.
    pub fn names(&self) -> Vec<&str> {
        builtin_names()
            .iter()
            .copied()
            .chain(self.user.iter().map(|t| t.name.as_str()))
            .collect()
    }

    /// The file's rows alone, in file order.
    pub fn user(&self) -> &[AgentType] {
        &self.user
    }
}

impl FileRow {
    fn into_type(self) -> Result<AgentType, AgentTypesError> {
        if !is_valid_name(&self.name) {
            return Err(AgentTypesError::InvalidName(self.name));
        }
        if builtin(&self.name).is_some() {
            return Err(AgentTypesError::ShadowsBuiltin(self.name));
        }
        // `acp:<command>` is the one harness spelling that is also a program: the same rule the
        // type selector uses, so a row's `harness = "acp:goose acp"` and a spawn of
        // `acp:goose acp` bind the same agent.
        let (harness, acp_agent) = if self.harness.starts_with(ACP_COMMAND_PREFIX) {
            match acp_command(&self.harness) {
                Some(t) => (Harness::Acp, t.acp_agent),
                None => {
                    return Err(AgentTypesError::UnknownHarness {
                        name: self.name,
                        harness: self.harness,
                    });
                }
            }
        } else {
            match self.harness.parse::<Harness>() {
                Ok(h) => (h, None),
                Err(_) => {
                    return Err(AgentTypesError::UnknownHarness {
                        name: self.name,
                        harness: self.harness,
                    });
                }
            }
        };
        let tools = self.tools.unwrap_or_default();
        if let Some(tool) = tools.iter().find(|t| *t != TOOL_READ && *t != TOOL_WRITE) {
            return Err(AgentTypesError::UnknownTool {
                name: self.name,
                tool: tool.clone(),
            });
        }
        if let Some(mode) = &self.approval_mode
            && harness != Harness::Acp
        {
            return Err(AgentTypesError::ApprovalModeOffAcp {
                name: self.name,
                harness: self.harness,
                mode: mode.clone(),
            });
        }
        if let Some(provider) = &self.provider
            && !crate::provider::valid_id(provider)
        {
            return Err(AgentTypesError::InvalidProvider {
                name: self.name,
                provider: provider.clone(),
            });
        }
        let credentials = self.credentials.unwrap_or_default();
        if let Some(bad) = credentials.iter().find(|c| {
            crate::provider::CredentialId::parse(c)
                .is_none_or(|id| Some(&id.provider) != self.provider.as_ref())
        }) {
            return Err(AgentTypesError::InvalidCredential {
                name: self.name,
                credential: bad.clone(),
            });
        }
        let profiles = match self.profile {
            None => Vec::new(),
            Some(ProfileKey::One(p)) => vec![p],
            Some(ProfileKey::Many(ps)) => ps,
        };
        if let Some(bad) = profiles.iter().find(|p| !is_valid_name(p)) {
            return Err(AgentTypesError::InvalidProfileName {
                name: self.name,
                profile: bad.clone(),
            });
        }
        Ok(AgentType {
            credentials,
            model: self.model,
            acp_agent,
            tools,
            prompt_prefix: self.prompt_prefix,
            approval_mode: self.approval_mode,
            provider: self.provider,
            profiles,
            ..AgentType::defaults(&self.name, &self.description, harness)
        })
    }
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
            t.name, "codex",
            "the plain name is canonical, -impl its alias"
        );
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
            ("acp-claude-acp", Harness::Acp),
            ("acp-codex-acp", Harness::Acp),
            ("acp-copilot", Harness::Acp),
            ("acp-fast-agent", Harness::Acp),
            ("acp-goose", Harness::Acp),
            ("acp-kilo", Harness::Acp),
            ("acp-opencode", Harness::Acp),
            ("acp-qwen", Harness::Acp),
            ("acp-vibe", Harness::Acp),
            ("acp-vtcode", Harness::Acp),
            ("claude", Harness::ClaudeCode),
            ("claude-orchestrator", Harness::ClaudeCode),
            ("cline", Harness::Cline),
            ("codex", Harness::Codex),
            ("copilot", Harness::Copilot),
            ("copilot-orchestrator", Harness::Copilot),
            ("gemini", Harness::Gemini),
            ("gemini-orchestrator", Harness::Gemini),
            ("goose", Harness::Goose),
            ("goose-orchestrator", Harness::Goose),
            ("opencode", Harness::OpenCode),
            ("pi", Harness::Pi),
            ("pi-orchestrator", Harness::Pi),
            ("qwen", Harness::Qwen),
            ("qwen-orchestrator", Harness::Qwen),
            ("agy", Harness::Antigravity),
            ("agy-orchestrator", Harness::Antigravity),
        ] {
            assert_eq!(builtin(name).unwrap().harness, h, "{name}");
            assert!(builtin_names().contains(&name), "{name} must be listed");
        }
        assert_eq!(
            builtin_names().len(),
            27,
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

    /// **Any ACP agent, by command.** `acp:<command>` resolves with no built-in naming it, the whole
    /// tail is carried as the agent selector, and the two degenerate spellings — a bare prefix and
    /// a prefix over whitespace — resolve to nothing rather than to a type with no program.
    #[test]
    fn an_acp_command_resolves_to_the_acp_row_with_the_command_as_its_agent() {
        let t = builtin("acp:copilot --acp").expect("a command is a type");
        assert_eq!(t.harness, Harness::Acp);
        assert_eq!(t.acp_agent.as_deref(), Some("copilot --acp"));
        assert_eq!(
            t.name, "acp:copilot --acp",
            "the name is what the operator wrote"
        );
        assert_eq!(t.model, None, "ACP chooses the model inside the session");
        assert!(
            !is_valid_name(&t.name),
            "a command line is not a name, and must never be taken for one"
        );
        // Surrounding whitespace is not part of the command.
        assert_eq!(
            builtin("acp:  goose acp ").unwrap().acp_agent.as_deref(),
            Some("goose acp")
        );
        for bare in ["acp:", "acp:   ", "acp", "acp-", "ACP:copilot"] {
            assert!(
                builtin(bare).is_none(),
                "`{bare}` names no program, so it is not a type"
            );
        }
        // A command whose first word is a refinement id is the same selector the built-in carries,
        // so `acp:opencode` and `acp-opencode` bind the same agent (the built-in adds a model).
        assert_eq!(
            builtin("acp:opencode").unwrap().acp_agent,
            builtin("acp-opencode").unwrap().acp_agent
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

    /// **A default model that names marion's canned plumbing is not applied to a live run.** The
    /// live node then names no model and the harness uses the operator's own default, exactly as
    /// it uses their login. A real vendor id (gemini's) is still a default either way.
    #[test]
    fn a_canned_plumbing_default_model_is_dropped_on_a_live_run() {
        for name in [
            "opencode",
            "copilot",
            "goose",
            "cline",
            "qwen",
            "acp-opencode",
        ] {
            let t = builtin(name).unwrap();
            assert_eq!(t.default_model(false), None, "{name}: live");
            assert_eq!(t.default_model(true), t.model, "{name}: canned keeps it");
        }
        let gemini = builtin("gemini").unwrap();
        assert_eq!(gemini.default_model(false), gemini.model);
        assert_eq!(builtin("claude").unwrap().default_model(false), None);
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
                "claude" | "gemini" | "copilot" | "qwen" | "agy" | "pi" => {
                    vec![TOOL_READ.into(), TOOL_WRITE.into()]
                }
                // `write` alone: goose's developer extension has no read-only tool to answer
                // `read` with, and the grant is the whole extension (S26).
                "goose" => vec![TOOL_WRITE.into()],
                _ => vec![],
            };
            assert_eq!(
                declared, expected,
                "{name}: the orchestrator types must stay read-only and the implementers must not \
                 lose the grant that closes item 24"
            );
        }
    }

    /// **A plain harness name is that harness's full implementer**, and `<harness>-impl` is the
    /// same type spelled the old way.
    ///
    /// Measured live: models picking a child reach for the plain name. When plain `claude` was a
    /// read-only orchestrator and plain `codex` an implementer, an opencode root spawned `claude`
    /// twice to edit a file, and the child could only call `spawn` itself and fail. One rule for
    /// every harness is what a model can guess.
    #[test]
    fn every_plain_harness_name_is_the_full_implementer_and_impl_is_its_alias() {
        for name in [
            "claude", "codex", "gemini", "opencode", "copilot", "goose", "cline", "qwen",
        ] {
            let plain = builtin(name).unwrap_or_else(|| panic!("{name} resolves"));
            assert_eq!(plain.name, name, "{name} is the canonical spelling");
            assert!(plain.writes_files(), "{name} must be able to change files");
            assert_eq!(
                builtin(&format!("{name}-impl")),
                Some(plain),
                "{name}-impl must stay a spelling of the same type, not a second definition"
            );
        }
    }

    /// **The read-only flavour has a name that says so**, on every harness where marion can
    /// actually withhold file writes, and it states its grant in its description.
    #[test]
    fn every_orchestrator_is_named_as_one_and_cannot_write() {
        let orchestrators: Vec<&str> = builtin_names()
            .iter()
            .copied()
            .filter(|n| n.ends_with("-orchestrator"))
            .collect();
        for (plain, h) in [
            ("claude", Harness::ClaudeCode),
            ("codex", Harness::Codex),
            ("gemini", Harness::Gemini),
            ("opencode", Harness::OpenCode),
            ("copilot", Harness::Copilot),
            ("goose", Harness::Goose),
            ("cline", Harness::Cline),
            ("qwen", Harness::Qwen),
        ] {
            // Only where the harness writes on a declaration alone can the grant be withheld; a
            // "read-only" codex type would be a lie, since codex writes under its own sandbox.
            let name = format!("{plain}-orchestrator");
            assert_eq!(
                orchestrators.contains(&name.as_str()),
                !h.writes_without_a_declaration(),
                "{name}"
            );
        }
        for name in orchestrators {
            let t = builtin(name).unwrap();
            assert!(t.tools.is_empty(), "{name}");
            assert!(!t.writes_files(), "{name}");
            assert!(
                t.description.starts_with("Orchestrator") && t.description.contains("cannot write"),
                "{name}: {}",
                t.description
            );
        }
    }

    /// **The listed names are the types, once each**: an alias is never listed beside its type,
    /// and no two types share a description — the picker and the `spawn` schema both print
    /// `name (description)` for every listed name, and a pair of identical lines is a pair a model
    /// cannot tell apart.
    #[test]
    fn each_listed_type_appears_once_with_a_description_of_its_own() {
        let names = builtin_names();
        let mut seen = std::collections::BTreeSet::new();
        for name in names {
            let t = builtin(name).unwrap();
            assert_eq!(&t.name, name, "{name}: an alias must not be listed");
            assert!(
                seen.insert(t.description.clone()),
                "{name}: duplicate description {:?}",
                t.description
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
        for (orchestrator, implementer) in [
            ("claude-orchestrator", "claude"),
            ("gemini-orchestrator", "gemini"),
            ("copilot-orchestrator", "copilot"),
            ("goose-orchestrator", "goose"),
            ("qwen-orchestrator", "qwen"),
            ("pi-orchestrator", "pi"),
        ] {
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
        for name in ["claude", "gemini"] {
            let t = builtin(name).unwrap();
            assert_eq!(
                t.tools,
                vec![TOOL_READ.to_string(), TOOL_WRITE.to_string()],
                "{name}: a node that may write and may not read is item 24 half-closed"
            );
        }
        for name in ["codex", "opencode"] {
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

    const REVIEWER: &str = r#"
[[agent]]
name = "reviewer"
harness = "codex"
model = "gpt-5-codex"
tools = ["read"]
description = "Reviews a diff and reports findings; never edits."
prompt_prefix = "You are a code reviewer. Do not modify files.\n\n"
"#;

    /// **A user row is an agent type.** Every field the file states lands on the resolved type,
    /// and every field it leaves unstated takes §3.1's default, exactly as a built-in does.
    #[test]
    fn parse_round_trips_a_reviewer_row() {
        let types = AgentTypes::parse(REVIEWER).expect("a well-formed row parses");
        let t = types.resolve("reviewer").expect("the row resolves by name");
        assert_eq!(t.name, "reviewer");
        assert_eq!(t.harness, Harness::Codex);
        assert_eq!(t.model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(t.tools, vec![TOOL_READ.to_string()]);
        assert_eq!(
            t.description,
            "Reviews a diff and reports findings; never edits."
        );
        assert_eq!(
            t.prompt_prefix.as_deref(),
            Some("You are a code reviewer. Do not modify files.\n\n")
        );
        assert_eq!(t.acp_agent, None);
        assert_eq!(t.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(t.max_depth, DEFAULT_MAX_DEPTH);
        assert_eq!(t.max_concurrent_children, DEFAULT_MAX_CONCURRENT_CHILDREN);
        assert_eq!(t.scope_ceiling, default_scope_ceiling());
        assert_eq!(types.user(), std::slice::from_ref(&t));
        // A row states only what it changes: the minimum is a name, a harness and a description.
        let minimal = AgentTypes::parse(
            "[[agent]]\nname = \"triager\"\nharness = \"claude-code\"\ndescription = \"Triages.\"\n",
        )
        .unwrap();
        let m = minimal.resolve("triager").unwrap();
        assert_eq!(m.model, None);
        assert_eq!(m.tools, Vec::<String>::new());
        assert_eq!(m.prompt_prefix, None);
        // Built-ins still resolve through the same table, unchanged.
        assert_eq!(types.resolve("codex-impl"), builtin("codex-impl"));
        assert_eq!(types.resolve("acp:goose acp"), builtin("acp:goose acp"));
        assert_eq!(types.resolve("nope"), None);
    }

    /// **`provider` names the endpoint a row's nodes talk to**, by registry id — carried verbatim,
    /// and refused at load where it cannot be an id at all. Whether the id is one the user has
    /// is the supervisor's check, which alone can read the user's `providers.toml`.
    #[test]
    fn a_rows_provider_is_carried_and_a_malformed_one_is_refused() {
        let t = AgentTypes::parse(
            "[[agent]]\nname = \"cheap\"\nharness = \"opencode\"\ndescription = \"d\"\n\
             provider = \"openrouter\"\nmodel = \"qwen/qwen3-coder\"\n",
        )
        .unwrap()
        .resolve("cheap")
        .unwrap();
        assert_eq!(t.provider.as_deref(), Some("openrouter"));
        assert_eq!(builtin("codex-impl").unwrap().provider, None);
        let err = AgentTypes::parse(
            "[[agent]]\nname = \"x\"\nharness = \"codex\"\ndescription = \"d\"\n\
             provider = \"Open Router\"\n",
        )
        .unwrap_err();
        assert!(
            matches!(err, AgentTypesError::InvalidProvider { .. }),
            "{err}"
        );
    }

    /// **`credentials` orders a row's keys for its provider** — ids of that provider only.
    #[test]
    fn a_rows_credentials_are_its_providers_own_ids() {
        let row = |extra: &str| {
            AgentTypes::parse(&format!(
                "[[agent]]\nname = \"x\"\nharness = \"codex\"\ndescription = \"d\"\n\
                 provider = \"openrouter\"\n{extra}"
            ))
        };
        let t = row("credentials = [\"openrouter:work\", \"openrouter\"]\n")
            .unwrap()
            .resolve("x")
            .unwrap();
        assert_eq!(t.credentials, vec!["openrouter:work", "openrouter"]);
        for bad in [
            "credentials = [\"groq:work\"]\n",
            "credentials = [\"Open:x\"]\n",
        ] {
            assert!(
                matches!(row(bad), Err(AgentTypesError::InvalidCredential { .. })),
                "{bad}"
            );
        }
    }

    /// **`approval_mode` is an ACP row's session mode**, carried verbatim, and refused by name on
    /// a harness with no session to set it in.
    #[test]
    fn an_approval_mode_parses_on_an_acp_row_and_is_refused_off_acp() {
        let types = AgentTypes::parse(
            "[[agent]]\nname = \"claude-acp-edits\"\nharness = \"acp:npx -y @agentclientprotocol/claude-agent-acp@0.66.0\"\ndescription = \"x\"\napproval_mode = \"acceptEdits\"\n",
        )
        .unwrap();
        let t = types.resolve("claude-acp-edits").unwrap();
        assert_eq!(t.harness, Harness::Acp);
        assert_eq!(t.approval_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(builtin("codex-impl").unwrap().approval_mode, None);
        let e = AgentTypes::parse(
            "[[agent]]\nname = \"c\"\nharness = \"codex\"\ndescription = \"x\"\napproval_mode = \"auto\"\n",
        )
        .unwrap_err();
        assert!(
            matches!(&e, AgentTypesError::ApprovalModeOffAcp { name, mode, .. } if name == "c" && mode == "auto"),
            "{e}"
        );
    }

    /// No file and an empty file are the same table: the built-ins, and nothing else.
    #[test]
    fn an_absent_or_empty_file_is_the_built_in_table() {
        let none = AgentTypes::builtins_only();
        let empty = AgentTypes::parse("").expect("an empty document is a file with no rows");
        let bare = AgentTypes::parse("agent = []\n").unwrap();
        for types in [&none, &empty, &bare] {
            assert!(types.user().is_empty());
            assert_eq!(types.names(), builtin_names().to_vec());
            for name in builtin_names() {
                assert_eq!(types.resolve(name), builtin(name), "{name}");
            }
        }
        // Every built-in has a prompt prefix of none: the field is the user file's alone.
        for name in builtin_names() {
            assert_eq!(builtin(name).unwrap().prompt_prefix, None, "{name}");
        }
    }

    /// A user row may not take a built-in's name **or one of its aliases**: `codex` is an alias of
    /// `codex-impl`, and a row named `codex` would silently rebind every spawn that spells it.
    #[test]
    fn a_row_named_after_a_builtin_or_alias_is_refused() {
        for name in [
            "codex-impl",
            "codex",
            "claude",
            "claude-orchestrator",
            "acp-opencode",
        ] {
            let text = format!(
                "[[agent]]\nname = \"{name}\"\nharness = \"gemini\"\ndescription = \"x\"\n"
            );
            assert_eq!(
                AgentTypes::parse(&text),
                Err(AgentTypesError::ShadowsBuiltin(name.to_string())),
                "{name}"
            );
        }
    }

    /// `harness` is §3.1's enum, or an ACP command. Anything else is refused naming what is known;
    /// `acp:` over nothing names no program and is refused too; `acp:<command>` lands on the ACP
    /// row with the command as its agent, exactly as the `acp:` type selector does.
    #[test]
    fn an_unknown_harness_is_refused_naming_the_known_ones() {
        let row = |harness: &str| {
            format!("[[agent]]\nname = \"r\"\nharness = \"{harness}\"\ndescription = \"x\"\n")
        };
        let e = AgentTypes::parse(&row("clod")).unwrap_err();
        assert_eq!(
            e,
            AgentTypesError::UnknownHarness {
                name: "r".into(),
                harness: "clod".into()
            }
        );
        let msg = e.to_string();
        for h in Harness::ALL {
            assert!(msg.contains(h.as_str()), "{msg} must name {h}");
        }
        assert!(msg.contains("acp:<command>"), "{msg}");
        assert!(msg.contains("known harnesses are claude "), "{msg}");
        // `claude` is the spelling a person types; `claude-code`, the wire's, still loads too.
        for spelling in ["claude", "claude-code"] {
            let t = AgentTypes::parse(&row(spelling)).unwrap();
            assert_eq!(
                t.resolve("r").unwrap().harness,
                Harness::ClaudeCode,
                "{spelling}"
            );
        }
        assert!(
            matches!(
                AgentTypes::parse(&row("acp:")),
                Err(AgentTypesError::UnknownHarness { .. })
            ),
            "a prefix over nothing is not a command"
        );
        let t = AgentTypes::parse(&row("acp:opencode acp")).unwrap();
        let t = t.resolve("r").unwrap();
        assert_eq!(t.harness, Harness::Acp);
        assert_eq!(t.acp_agent.as_deref(), Some("opencode acp"));
    }

    /// `tools` is §3.1's allowlist in marion's two-word vocabulary; a word outside it is refused by
    /// name rather than passed to an adapter that would refuse it at launch, or — worse — to a
    /// harness that ignores it. An unknown key is a load error, never a silent default (§3.1).
    #[test]
    fn an_unknown_tool_or_field_is_refused() {
        let e = AgentTypes::parse(
            "[[agent]]\nname = \"r\"\nharness = \"codex\"\ndescription = \"x\"\ntools = [\"bash\"]\n",
        )
        .unwrap_err();
        assert_eq!(
            e,
            AgentTypesError::UnknownTool {
                name: "r".into(),
                tool: "bash".into()
            }
        );
        let msg = e.to_string();
        assert!(msg.contains(TOOL_READ) && msg.contains(TOOL_WRITE), "{msg}");
        let e = AgentTypes::parse(
            "[[agent]]\nname = \"r\"\nharness = \"codex\"\ndescription = \"x\"\ntimeout_secs = 5\n",
        )
        .unwrap_err();
        assert!(matches!(e, AgentTypesError::Syntax(_)), "{e:?}");
        assert!(e.to_string().contains("timeout_secs"), "{e}");
        assert!(matches!(
            AgentTypes::parse("[[agent]\n"),
            Err(AgentTypesError::Syntax(_))
        ));
    }

    /// One name, one definition — within the file as much as against the built-ins. And a name
    /// is §3.1's name: a row that spells a path or a flag is refused before it can become one.
    #[test]
    fn a_duplicate_name_within_the_file_is_refused() {
        let twice = "[[agent]]\nname = \"r\"\nharness = \"codex\"\ndescription = \"x\"\n\
                     [[agent]]\nname = \"r\"\nharness = \"gemini\"\ndescription = \"y\"\n";
        assert_eq!(
            AgentTypes::parse(twice),
            Err(AgentTypesError::Duplicate("r".into()))
        );
        for bad in ["", "-r", "a/b", "acp:goose acp", "r r"] {
            let text =
                format!("[[agent]]\nname = \"{bad}\"\nharness = \"codex\"\ndescription = \"x\"\n");
            assert_eq!(
                AgentTypes::parse(&text),
                Err(AgentTypesError::InvalidName(bad.to_string())),
                "{bad:?}"
            );
        }
    }

    /// `names()` is what a picker and an error message list: the built-ins in their documented
    /// order, then the file's rows in the order the operator wrote them.
    #[test]
    fn names_lists_builtins_then_user_rows() {
        let text = format!(
            "{REVIEWER}\n[[agent]]\nname = \"triager\"\nharness = \"gemini\"\ndescription = \"t\"\n"
        );
        let types = AgentTypes::parse(&text).unwrap();
        let mut expected: Vec<&str> = builtin_names().to_vec();
        expected.extend(["reviewer", "triager"]);
        assert_eq!(types.names(), expected);
        assert_eq!(
            types
                .user()
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["reviewer", "triager"]
        );
    }

    /// **`profile` names the account a node runs on**: one name, or a list that is the failover
    /// order for an expired or refused login only. Each name is checked like a type name, since it
    /// becomes a directory.
    #[test]
    fn profile_is_one_name_or_an_ordered_list_of_names() {
        let row = |profile: &str| {
            format!(
                "[[agent]]\nname = \"w\"\nharness = \"claude-code\"\ndescription = \"d\"\n{profile}"
            )
        };
        let one = AgentTypes::parse(&row("profile = \"work\"\n")).unwrap();
        assert_eq!(one.resolve("w").unwrap().profiles, ["work"]);
        let many = AgentTypes::parse(&row("profile = [\"work\", \"personal\"]\n")).unwrap();
        assert_eq!(many.resolve("w").unwrap().profiles, ["work", "personal"]);
        let none = AgentTypes::parse(&row("")).unwrap();
        assert!(none.resolve("w").unwrap().profiles.is_empty());
        assert!(builtin("claude").unwrap().profiles.is_empty());
        assert_eq!(
            AgentTypes::parse(&row("profile = \"../up\"\n")),
            Err(AgentTypesError::InvalidProfileName {
                name: "w".into(),
                profile: "../up".into()
            })
        );
    }
}
