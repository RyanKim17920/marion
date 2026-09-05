//! The opencode adapter — the `run` surface (design §6.4, fixture `tests/fixtures/s13/`).
//!
//! Measured against **opencode 1.17.3**. `opencode run --pure --format json` is a full agent turn
//! as NDJSON on stdout, over pipes, binding **no TCP port** — so it is `LaunchOnly` with
//! `ProtocolEvents`, exactly like `codex exec`, and none of §6.4's `opencode serve` paragraph
//! (`prompt_async`, `/event`, `OPENCODE_EXPERIMENTAL_EVENT_SYSTEM`) applies to it.
//!
//! The isolation env below is not hygiene. `OPENCODE_CONFIG` and `OPENCODE_CONFIG_CONTENT` both
//! **merge on top of** the operator's global config rather than replacing it, so `XDG_CONFIG_HOME`
//! is the only true config replacement — and the two `OPENCODE_DISABLE_CLAUDE_CODE` /
//! `OPENCODE_DISABLE_EXTERNAL_SKILLS` vars close a hazard `inherit_user_config: false` never
//! contemplated (see [`isolation_env`]).

use std::path::{Path, PathBuf};

use marion_core::agent_type;
use marion_core::harness::Harness;
use serde_json::{Value, json};

use crate::grammar::{
    Cond, Failure, Name, OnRefusedReport, Pairing, StreamGrammar, Verdict, Where,
};
pub use crate::mcp_bridge::BridgeEnv;
use crate::spec::{
    Arg, Constraint, Env, Field, HarnessSpec, McpRoute, McpRoutes, Spelling, Surfaces,
    ToolSpelling, Val, When,
};

/// `$XDG_CONFIG_HOME`'s name under the sandbox — one spelling for [`SPEC`]'s env row and for
/// [`config_path`], so the document is written where the relocated root is read from.
const XDG_CONFIG_DIR: &str = "config";

/// opencode's row. Measured against 1.17.3 (`tests/fixtures/s13/`). `LaunchOnly`: `run --pure
/// --format json` is a full agent turn as NDJSON over pipes, binding no port.
///
/// **The env is not hygiene, and the split between `Canned` and `Always` is exactly where S13 puts
/// the credential.** Auth lives in `$XDG_DATA_HOME/opencode/auth.json` and config in
/// `$XDG_CONFIG_HOME/opencode/`, resolved through **different variables**, with `HOME`
/// independently driving the `~/.claude`, `~/.agents` and `~/.opencode` lookups. Relocating any of
/// them hides the login a live node is meant to present, so the five relocations are `Canned`;
/// the `OPENCODE_DISABLE_*` set and `OPENCODE_DB=:memory:` are kept under both modes because they
/// were never the isolation — and two of them matter *more* live, not less:
/// `OPENCODE_DISABLE_CLAUDE_CODE` / `OPENCODE_DISABLE_EXTERNAL_SKILLS` sever a route no `XDG_*`
/// var ever closed (`~/.claude/CLAUDE.md` and `~/.claude/skills/**` are found via `HOME`, and
/// under live that `HOME` is the operator's real one). The rest suppress work a spawned node has
/// no business doing: a boot models.dev fetch plus a 60-minute in-process loop, an LSP toolchain
/// download, an auto-updater that pipes an install script into `bash`, session sharing, and an
/// on-disk sqlite file.
///
/// **What this env cannot do is bound the run.** opencode *never exits* on a provider hang: S13
/// measured a 500 still retrying at 90 s and a connection-refused still hung at 180 s. The provider
/// timeouts in [`config_json`] are a first line of defence, not a substitute — marion's own bounded
/// run and §9's two-step group kill are load-bearing for an opencode child.
pub const SPEC: HarnessSpec = HarnessSpec {
    harness: Harness::OpenCode,
    surfaces: Surfaces::LaunchOnly,
    program: Some("opencode"),
    argv: &[
        Arg::Lit("run"),
        // Skips loading external plugins. It does *not* gate the forkDetach'ed
        // `@opencode-ai/plugin` npm install or the ripgrep auto-download (S13) — those are bounded
        // by the throwaway HOME below, not by a flag.
        Arg::Lit("--pure"),
        Arg::Lit("--format"),
        Arg::Lit("json"),
        // **Required.** Without `--title` opencode issues an extra `You are a title generator`
        // request against `small_model` — S13 measured 3 POSTs instead of 2.
        Arg::Flag("--title", Field::Title),
        // `provider/model`, the only form `-m` accepts; the adapter parses it once so the config's
        // provider block and argv can never name different providers.
        Arg::Flag("-m", Field::Model),
        // Positional (variadic). stdin works identically, but a LaunchOnly surface gives the child
        // no stdin.
        Arg::Pos(Field::Prompt),
    ],
    pane: None,
    env: &[
        Env {
            key: "HOME",
            val: Val::Under(""),
            when: When::Canned,
        },
        Env {
            key: "XDG_CONFIG_HOME",
            val: Val::Under(XDG_CONFIG_DIR),
            when: When::Canned,
        },
        Env {
            key: "XDG_DATA_HOME",
            val: Val::Under("data"),
            when: When::Canned,
        },
        Env {
            key: "XDG_CACHE_HOME",
            val: Val::Under("cache"),
            when: When::Canned,
        },
        Env {
            key: "XDG_STATE_HOME",
            val: Val::Under("state"),
            when: When::Canned,
        },
        Env {
            key: "OPENCODE_DISABLE_CLAUDE_CODE",
            val: Val::Lit("1"),
            when: When::Always,
        },
        Env {
            key: "OPENCODE_DISABLE_EXTERNAL_SKILLS",
            val: Val::Lit("1"),
            when: When::Always,
        },
        Env {
            key: "OPENCODE_DISABLE_PROJECT_CONFIG",
            val: Val::Lit("1"),
            when: When::Always,
        },
        Env {
            key: "OPENCODE_DISABLE_MODELS_FETCH",
            val: Val::Lit("1"),
            when: When::Always,
        },
        Env {
            key: "OPENCODE_DISABLE_LSP_DOWNLOAD",
            val: Val::Lit("1"),
            when: When::Always,
        },
        Env {
            key: "OPENCODE_DISABLE_AUTOUPDATE",
            val: Val::Lit("1"),
            when: When::Always,
        },
        Env {
            key: "OPENCODE_DISABLE_SHARE",
            val: Val::Lit("1"),
            when: When::Always,
        },
        Env {
            key: "OPENCODE_DB",
            val: Val::Lit(":memory:"),
            when: When::Always,
        },
        // **`cwd` alone does not place an opencode node, and the difference is a containment
        // failure.** Measured against 1.17.3 through marion's own `spawn`: with `Invocation.cwd`
        // set to the child's worktree and `PWD` left inherited, the child's `bash` tool reported
        // `pwd` / `git rev-parse --show-toplevel` as the **operator's own repository** and a
        // `write` of `src/…` landed there. Everything downstream of that is silently wrong rather
        // than loud: §6.7 derives `changed_paths` from a git diff of the worktree, which the child
        // never touched, so the contract records a clean audit of a run that wrote outside every
        // scope it was given. Under both modes, because it is placement and not isolation.
        Env {
            key: "PWD",
            val: Val::Field(Field::Cwd),
            when: When::Always,
        },
        // The live route: marion's MCP declaration inline, merged over the operator's own config
        // ([`CONFIG_CONTENT_ENV`], [`live_config_json`]). Absent where the node has no bridge.
        Env {
            key: CONFIG_CONTENT_ENV,
            val: Val::Field(Field::InlineConfig),
            when: When::Always,
        },
    ],
    stream: Some(&STREAM),
    // `read` → `read`, `write` → `write`: the names opencode already declares. s14 measured
    // 1.17.3's default tool list as `bash, edit, glob, grep, read, skill, task, todowrite, webfetch,
    // write`, so a declaration here is **satisfied rather than newly granted** and argv carries
    // nothing for it. A spelling collision, not a shared vocabulary — the mapping is written out so
    // a future marion verb cannot pass through unmapped. `OPENCODE_PERMISSION` *does* gate (s14:
    // `{"read":"deny"}` took the schema from 10 tools to 9), which is precisely why marion compiles
    // nothing into it: driving it off the declaration would silently narrow every opencode node.
    tool_names: &[
        (agent_type::TOOL_READ, "read"),
        (agent_type::TOOL_WRITE, "write"),
    ],
    // `<serverName>_<toolName>` (S13, verified live). The JSON-RPC `tools/call` opencode then makes
    // carries the **unprefixed** `report`: that is the MCP wire layer, not the model-facing name.
    spelling: Spelling::Fixed(ToolSpelling::ServerUnderscoreTool),
    // A file under an isolated `$XDG_CONFIG_HOME` when marion owns the config surface, and inline
    // `OPENCODE_CONFIG_CONTENT` when the operator does.
    mcp: McpRoutes {
        canned: McpRoute::Document,
        live: McpRoute::Environment(CONFIG_CONTENT_ENV),
    },
    constraint: Constraint::Fixed {
        prefix: "",
        value: NO_COMPILED_TOOL_CONSTRAINT,
    },
    note: "S13 on opencode 1.17.3: the run surface, the exhaustive OPENCODE_* scan behind the env, \
           the PWD placement measured through marion's own spawn; harness_matrix's opencode cell \
           runs this row end to end",
};

/// How an `opencode run --pure --format json` stream is read (`tests/fixtures/s13/`).
///
/// The event set is closed and measured: `step_start | step_finish | text | reasoning | tool_use |
/// error`. **There is no terminal frame** — the stream simply ends when the session goes idle — so
/// this is a fold over whatever arrived and nothing here waits for a last event. `tool_use` fires
/// **only on terminal states** (`completed` / `error`) and carries the call's `input` under
/// `part.state.input`, so the verdict is on the call unit itself, and a state that is neither is
/// one this harness has not been measured emitting.
///
/// **opencode is the one harness whose refusal shape is recorded rather than constructed**: S13
/// measured `{"status":"error","error":"The user rejected permission to use this specific tool
/// call."}` on the tool part, with the run continuing and exiting 0 — the silent success §6.1 step
/// 8 exists to refuse, which is why a refused `report` fails the run here and reads no narrative.
/// `error` frames carry `{name, data:{message, …}}`, measured arriving with **exit 1 and an empty
/// stderr**, so the stream is the only place that failure is described.
pub const STREAM: StreamGrammar = StreamGrammar {
    call: Where {
        frame: &[Cond::Eq("/type", "tool_use")],
        each: None,
        unit: &[],
    },
    name: Name::Prefixed("/part/tool"),
    args: "/part/state/input",
    pairing: Pairing::SameUnit {
        id: None,
        verdict: Verdict::Terminal {
            path: "/part/state/status",
            ok: "completed",
            err: "error",
            words: &["/part/state/error"],
            fallback: "the tool part carried no message",
        },
    },
    refused_report: OnRefusedReport::FailWithoutNarrative,
    failures: &[Failure::Frame {
        at: Where {
            frame: &[Cond::Eq("/type", "error")],
            each: None,
            unit: &[],
        },
        words: &["/error/data/message", "/error/name"],
        fallback: "the child's stream carried an error frame",
    }],
    file_changes: None,
};

/// The MCP server alias. opencode exposes MCP tools to the model as `<serverName>_<toolName>`, so
/// this alias is literally half of `marion_report`.
pub const MCP_ALIAS: &str = crate::spec::MCP_ALIAS;

/// Inline JSONC config text, a virtual source that is **never written back** to disk.
///
/// It is last in S13's merge order and **merges over** whatever config already resolved — which is
/// exactly wrong for isolation, and exactly right for [`Auth::Inherited`]: a live node keeps the
/// operator's own provider, models and credentials and receives marion's MCP declaration on top.
/// The same property that makes it useless as a sandbox makes it the live route.
pub const CONFIG_CONTENT_ENV: &str = "OPENCODE_CONFIG_CONTENT";

/// `provider.<id>.options.timeout`, ms. A **first line of defence only** — see [`isolation_env`]'s
/// note on the hang.
pub const PROVIDER_TIMEOUT_MS: u64 = 120_000;
/// `provider.<id>.options.headerTimeout`, ms: how long to wait for the first response header,
/// which is the knob a connection that never answers actually trips.
pub const PROVIDER_HEADER_TIMEOUT_MS: u64 = 30_000;

/// A `provider/model` pair, which is the only form `-m` accepts and the form the config `model`
/// key repeats. Split once so the provider block and the argv can never name different providers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

impl ModelRef {
    /// `None` when the string is not `provider/model`. There is **no `OPENCODE_MODEL` env var**
    /// (exhaustive `OPENCODE_*` scan of the binary in S13), so this string is the only channel.
    pub fn parse(s: &str) -> Option<Self> {
        let (provider, model) = s.split_once('/')?;
        if provider.is_empty() || model.is_empty() || model.contains('/') {
            return None;
        }
        Some(Self {
            provider: provider.to_string(),
            model: model.to_string(),
        })
    }

    /// The `provider/model` spelling, as argv and as the config `model` key.
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

/// `$XDG_CONFIG_HOME` — the only true config replacement (S13).
pub fn xdg_config_home(sandbox: &Path) -> PathBuf {
    sandbox.join(XDG_CONFIG_DIR)
}

/// Where [`config_json`] is written: the first entry of opencode's merge order.
pub fn config_path(sandbox: &Path) -> PathBuf {
    xdg_config_home(sandbox)
        .join("opencode")
        .join("opencode.json")
}

/// Everything the generated `opencode.json` needs that is not the MCP block.
#[derive(Debug, Clone)]
pub struct ConfigSpec {
    pub model: ModelRef,
    /// The provider base URL, in marion's canonical `…/v1` form — which is what
    /// `@ai-sdk/openai-compatible` wants **verbatim**: it appends `/chat/completions` (S13,
    /// confirmed on the wire). No `/v1` derivation here, unlike gemini and Claude Code.
    pub base_url: String,
    pub api_key: Option<String>,
}

/// The opencode config document, written under `$XDG_CONFIG_HOME/opencode/`.
///
/// The schema (`https://opencode.ai/config.json`, `$defs.McpLocalConfig`) sets
/// **`additionalProperties: false`**, so a wrong key name is a hard failure rather than a
/// quietly-ignored field. Three names that are easy to get wrong, all measured in S13:
/// `type` is required and its enum is `["local"]`; `command` is an **argv array**, not a string;
/// and the env block is spelled **`environment`** — `env` is *gemini's* spelling.
///
/// `@ai-sdk/openai-compatible` is **bundled in the binary**, so naming it in `npm` costs no fetch.
pub fn config_json(spec: &ConfigSpec, mcp: Option<&BridgeEnv>) -> Value {
    let qualified = spec.model.qualified();
    let mut options = json!({
        // camelCase, per the schema; `baseUrl` would be silently unused.
        "baseURL": spec.base_url,
        // A hang here is unbounded (see `isolation_env`), so these are set even though marion's own
        // bounded run is what actually ends such a child.
        "timeout": PROVIDER_TIMEOUT_MS,
        "headerTimeout": PROVIDER_HEADER_TIMEOUT_MS,
    });
    if let Some(k) = &spec.api_key {
        options["apiKey"] = json!(k);
    }

    // Built through a `Map` rather than `json!` because both keys are values, not literals.
    let mut models = serde_json::Map::new();
    models.insert(
        spec.model.model.clone(),
        json!({ "name": spec.model.model, "tool_call": true }),
    );
    let mut providers = serde_json::Map::new();
    providers.insert(
        spec.model.provider.clone(),
        json!({
            "npm": "@ai-sdk/openai-compatible",
            "name": spec.model.provider,
            "options": options,
            "models": Value::Object(models),
        }),
    );

    let mut config = json!({
        "model": qualified,
        // The title-generation and summary model. Pinned to the same one so a node can never
        // reach a second provider marion did not configure.
        "small_model": qualified,
        "provider": Value::Object(providers),
    });

    if let Some(b) = mcp {
        config["mcp"] = json!({ MCP_ALIAS: mcp_block(b) });
    }
    config
}

/// What §6.7's `allowed_tools` records for an opencode node: **marion compiled no tool constraint
/// at all, and the harness's own defaults were in force.**
///
/// Measured against [`config_json`] directly above, which is the entire document marion writes:
/// it carries `model`, `small_model`, `provider` and `mcp` and **nothing else**. No `tools` block,
/// no `permission` block, no `--agent` in [`compile_run`]'s argv. That is also why an opencode
/// child declares `write`, `edit` and `bash` without marion asking — they are opencode's defaults,
/// not a grant marion made.
///
/// **Why not an empty list.** `[]` in this field reads as *"no tool was allowed"*, which is the
/// strongest possible understatement of a node that could run `bash` — and understating what a
/// child was permitted is precisely the failure §6.7's audit record exists to prevent. An absence
/// of constraint and an absence of permission are opposite facts, and `Vec<String>` spells them
/// the same way unless one of them is named.
///
/// **Why not an opencode-native spelling.** `agent:build` or `permission:allow` would look like
/// the other three harnesses' records and would assert that marion compiled a constraint it never
/// wrote — the accept-and-ignore shape one layer up, in the field that exists to catch it. This
/// says the true thing instead, and is greppable the day opencode grows a surface marion drives.
///
/// It is a claim about **this axis only**. An opencode child is still bounded by its worktree, its
/// wall clock, and §6.7's scope check; none of those are tool permissions.
pub const NO_COMPILED_TOOL_CONSTRAINT: &str = "harness-default:unconstrained";

/// The `mcp.<alias>` entry, which is identical on both routes: the declaration marion makes does
/// not change because it travelled by environment instead of by file.
///
/// The schema (`https://opencode.ai/config.json`, `$defs.McpLocalConfig`) sets
/// `additionalProperties: false`, so `env` instead of `environment`, or a `command` string instead
/// of an argv array, is a hard failure rather than an ignored field.
fn mcp_block(b: &BridgeEnv) -> Value {
    let mut command: Vec<String> = vec![b.bridge.to_string_lossy().into_owned()];
    command.extend(b.args.iter().cloned());
    json!({
        "type": "local",
        "command": command,
        "environment": b.env_json(),
        "enabled": true,
    })
}

/// The document [`CONFIG_CONTENT_ENV`] carries on a live node: **marion's MCP declaration and
/// nothing else**.
///
/// No `provider` block and no `small_model`, and both omissions are deliberate rather than
/// minimalism. This text is *merged over* the operator's own config (S13), so a `provider` entry
/// would shadow the real provider the node is meant to authenticate against, and `small_model`
/// would repoint their title/summary model at it too — marion overriding the model an operator
/// chose, on a run whose whole premise is that the operator's own setup is in charge. `model` is
/// likewise absent: `-m` already carries it and takes precedence over config.
pub fn live_config_json(mcp: Option<&BridgeEnv>) -> Value {
    let mut config = json!({});
    if let Some(b) = mcp {
        config["mcp"] = json!({ MCP_ALIAS: mcp_block(b) });
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::contract::AgentId;

    use crate::adapter::{
        Extras, HarnessAdapter, LaunchSpec, McpDeclaration, OpenCodeAdapter, SpawnCtx,
    };
    use crate::auth::Auth;
    use crate::invocation::Invocation;

    fn model() -> ModelRef {
        ModelRef::parse("canned/canned-1").unwrap()
    }

    fn ctx() -> SpawnCtx {
        SpawnCtx {
            agent_id: AgentId("019f-child".into()),
            agent_type: "opencode".into(),
            depth: 1,
            node_token: None,
            ready_file: None,
            repo: "/repo".into(),
            state_dir: "/state".into(),
            bridge: "/bin/marion-supervisor".into(),
            bridge_args: vec!["mcp".into()],
        }
    }

    fn spec() -> LaunchSpec {
        LaunchSpec {
            cwd: "/tmp/wt".into(),
            model: Some("canned/canned-1".into()),
            prompt: "do the task".into(),
            tools: vec![],
            allowed_tools: vec![],
            mcp: McpDeclaration::Marion,
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            api_key: Some("sk-fake".into()),
            auth: Auth::Canned,
            config_dir: "/tmp/sb".into(),
            extra: Extras::default(),
        }
    }

    /// The same node under `--live`: nothing relocated, the operator's own provider, and the MCP
    /// declaration inline.
    fn live_spec() -> LaunchSpec {
        LaunchSpec {
            auth: Auth::Inherited,
            model: Some("anthropic/claude-sonnet-4-5".into()),
            base_url: None,
            api_key: None,
            ..spec()
        }
    }

    fn compile_run(spec: &LaunchSpec) -> Invocation {
        OpenCodeAdapter.compile(spec, &ctx()).unwrap()
    }

    fn bridge() -> BridgeEnv {
        BridgeEnv {
            bridge: "/bin/marion-supervisor".into(),
            args: vec!["mcp".into()],
            repo: "/repo".into(),
            state: "/state".into(),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
            agent_id: AgentId("019f-child".into()),
            agent_type: "opencode".into(),
            depth: 1,
            node_token: None,
            ready_file: None,
        }
    }

    fn cfg() -> ConfigSpec {
        ConfigSpec {
            model: model(),
            base_url: "http://127.0.0.1:8099/v1".into(),
            api_key: Some("sk-fake".into()),
        }
    }

    #[test]
    fn run_is_pure_json_and_the_prompt_is_positional() {
        let inv = compile_run(&spec());
        assert_eq!(inv.program, "opencode");
        assert_eq!(inv.args[0], "run");
        assert!(inv.args.iter().any(|a| a == "--pure"));
        let f = inv.args.iter().position(|a| a == "--format").unwrap();
        assert_eq!(inv.args[f + 1], "json");
        assert_eq!(inv.args.last().unwrap(), "do the task");
    }

    /// Measured: 2 POSTs with `--title`, 3 without — the extra one is a title-generation call
    /// against `small_model` (`tests/fixtures/s13/`).
    #[test]
    fn a_title_is_passed_so_no_extra_model_call_is_made() {
        let inv = compile_run(&spec());
        let i = inv
            .args
            .iter()
            .position(|a| a == "--title")
            .expect("without it opencode makes a third POST to generate one");
        assert_eq!(inv.args[i + 1], "marion-019f-child");
    }

    #[test]
    fn the_model_is_the_provider_slash_model_form() {
        let inv = compile_run(&spec());
        let i = inv.args.iter().position(|a| a == "-m").unwrap();
        assert_eq!(inv.args[i + 1], "canned/canned-1");
        assert_eq!(ModelRef::parse("canned/canned-1"), Some(model()));
        for bad in ["canned", "/m", "p/", "a/b/c", ""] {
            assert!(ModelRef::parse(bad).is_none(), "{bad}");
        }
    }

    /// **The node is placed by `PWD` as well as by `cwd`, and one without the other is not
    /// placement at all.** Measured against 1.17.3 through `spawn`: with only `Invocation.cwd` set,
    /// the child's `bash` reported `pwd` and `git rev-parse --show-toplevel` as the directory
    /// *marion* was launched from, and a `write` of `src/…` landed in the operator's own
    /// repository. Everything downstream is then quietly wrong rather than loud — §6.7 derives
    /// `changed_paths` from a diff of the worktree, which such a child never touches, so the
    /// contract records an empty, unviolated, `scope_enforced: true` audit of a run that wrote
    /// outside every scope it was given.
    ///
    /// Asserted under **both** modes: this is where the node works, which is not part of the
    /// canned-mode isolation that live mode drops.
    #[test]
    fn the_node_is_placed_in_its_own_cwd_by_pwd_too_not_only_by_chdir() {
        for (label, inv) in [
            ("canned", compile_run(&spec())),
            ("live", compile_run(&live_spec())),
        ] {
            let pwd = inv
                .env
                .iter()
                .find(|(k, _)| k == "PWD")
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| {
                    panic!(
                        "{label}: PWD is unset, so the child re-enters whatever directory marion \
                         was launched from and edits it instead of its worktree. env: {:?}",
                        inv.env
                    )
                });
            assert_eq!(
                pwd,
                inv.cwd.to_string_lossy(),
                "{label}: PWD and cwd must name one directory; two answers to \"where am I\" is \
                 the same failure as having only the wrong one"
            );
        }
    }

    /// The cross-harness contamination sever. Without these two an opencode child reads
    /// `~/.claude/CLAUDE.md` and `~/.claude/skills/**/SKILL.md` — a *different* harness's user
    /// config — and no `XDG_*` var prevents it (`tests/fixtures/s13/`).
    #[test]
    fn the_claude_code_adoption_is_severed() {
        let inv = compile_run(&spec());
        for k in [
            "OPENCODE_DISABLE_CLAUDE_CODE",
            "OPENCODE_DISABLE_EXTERNAL_SKILLS",
        ] {
            assert_eq!(
                inv.env
                    .iter()
                    .find(|(n, _)| n == k)
                    .map(|(_, v)| v.as_str()),
                Some("1"),
                "{k}: without it the child inherits another harness's user configuration"
            );
        }
    }

    /// `OPENCODE_CONFIG` and `OPENCODE_CONFIG_CONTENT` merge rather than replace, so isolation is
    /// all five variables or none of it.
    #[test]
    fn home_and_all_four_xdg_roots_are_relocated() {
        let inv = compile_run(&spec());
        let get = |k: &str| {
            inv.env
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("{k} must be set"))
        };
        assert_eq!(get("HOME"), "/tmp/sb");
        assert_eq!(get("XDG_CONFIG_HOME"), "/tmp/sb/config");
        assert_eq!(get("XDG_DATA_HOME"), "/tmp/sb/data");
        assert_eq!(get("XDG_CACHE_HOME"), "/tmp/sb/cache");
        assert_eq!(get("XDG_STATE_HOME"), "/tmp/sb/state");
        assert_eq!(get("OPENCODE_DB"), ":memory:");
    }

    #[test]
    fn the_generated_config_sits_where_the_isolated_config_root_is_read_from() {
        let inv = compile_run(&spec());
        let (_, xdg) = inv
            .env
            .iter()
            .find(|(k, _)| k == "XDG_CONFIG_HOME")
            .unwrap();
        assert!(config_path(Path::new("/tmp/sb")).starts_with(xdg));
        assert_eq!(
            config_path(Path::new("/tmp/sb")),
            PathBuf::from("/tmp/sb/config/opencode/opencode.json")
        );
    }

    /// `additionalProperties: false`: `env` instead of `environment`, or a `command` string
    /// instead of an argv array, is a hard schema failure rather than an ignored field.
    #[test]
    fn the_mcp_block_uses_the_exact_key_names_the_schema_allows() {
        let v = config_json(&cfg(), Some(&bridge()));
        let m = &v["mcp"]["marion"];
        assert_eq!(m["type"], json!("local"), "required, enum is [\"local\"]");
        assert_eq!(
            m["command"],
            json!(["/bin/marion-supervisor", "mcp"]),
            "an ARGV ARRAY, never a string"
        );
        assert!(
            m["environment"].is_object(),
            "the key is `environment`; `env` is gemini's spelling and fails the schema"
        );
        assert!(m["env"].is_null());
        assert_eq!(m["enabled"], json!(true));
    }

    #[test]
    fn the_bridge_declaration_carries_the_nodes_identity() {
        let mut b = bridge();
        b.ready_file = Some("/state/x/mcp-ready".into());
        let v = config_json(&cfg(), Some(&b));
        let e = &v["mcp"]["marion"]["environment"];
        assert_eq!(e["MARION_AGENT_ID"], json!("019f-child"));
        assert_eq!(e["MARION_READY_FILE"], json!("/state/x/mcp-ready"));
        assert_eq!(e["MARION_REPO"], json!("/repo"));
        assert_eq!(e["MARION_BASE_URL"], json!("http://127.0.0.1:8099/v1"));
    }

    #[test]
    fn the_provider_block_names_the_bundled_openai_compatible_adapter() {
        let v = config_json(&cfg(), Some(&bridge()));
        let p = &v["provider"]["canned"];
        assert_eq!(
            p["npm"],
            json!("@ai-sdk/openai-compatible"),
            "bundled: no fetch"
        );
        // camelCase, and the `/v1` is kept: the SDK appends `/chat/completions` to it.
        assert_eq!(p["options"]["baseURL"], json!("http://127.0.0.1:8099/v1"));
        assert_eq!(p["options"]["apiKey"], json!("sk-fake"));
        assert_eq!(p["models"]["canned-1"]["tool_call"], json!(true));
        assert_eq!(v["model"], json!("canned/canned-1"));
        assert_eq!(
            v["small_model"], v["model"],
            "the title/summary model must not be a second provider"
        );
    }

    /// A hang the harness never resolves is what these bound *first*; marion's bounded run is what
    /// actually ends the child (S13: 500 retrying at 90 s, connection-refused hung at 180 s).
    #[test]
    fn the_provider_carries_the_timeouts_that_are_the_first_line_of_defence() {
        let v = config_json(&cfg(), Some(&bridge()));
        let o = &v["provider"]["canned"]["options"];
        assert_eq!(o["timeout"], json!(PROVIDER_TIMEOUT_MS));
        assert_eq!(o["headerTimeout"], json!(PROVIDER_HEADER_TIMEOUT_MS));
    }

    #[test]
    fn a_config_without_an_mcp_server_still_configures_the_provider() {
        let v = config_json(&cfg(), None);
        assert!(v["mcp"].is_null());
        assert_eq!(v["model"], json!("canned/canned-1"));
    }

    /// **Live mode stops relocating, and stops there.** The five relocations are exactly what hides
    /// the operator's login — `auth.json` lives under `$XDG_DATA_HOME`, and `HOME` drives the rest
    /// — so all five go, asserted by name.
    #[test]
    fn a_live_opencode_node_relocates_neither_home_nor_any_xdg_root() {
        let inv = compile_run(&live_spec());
        for k in [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME",
        ] {
            assert!(
                !inv.env.iter().any(|(n, _)| n == k),
                "{k} must be absent under --live: opencode reads auth.json out of $XDG_DATA_HOME \
                 and the rest out of $HOME, so relocating any of them hides the login. env: {:?}",
                inv.env
            );
        }
        let strip_model = |inv: &Invocation| -> Vec<String> {
            let m = inv.args.iter().position(|a| a == "-m").unwrap();
            let mut args = inv.args.clone();
            args.remove(m + 1);
            args
        };
        assert_eq!(
            strip_model(&inv),
            strip_model(&compile_run(&spec())),
            "live differs from canned in env and in whose provider the model names"
        );
    }

    /// The hygiene set is **kept**, and the two contamination severs matter *more* live, not less:
    /// under `--live` the `HOME` an unsevered child reads `~/.claude/CLAUDE.md` and
    /// `~/.claude/skills/**` from is the operator's real one (`tests/fixtures/s13/`).
    #[test]
    fn a_live_node_still_severs_the_claude_code_adoption_and_the_rest_of_the_hygiene() {
        let inv = compile_run(&live_spec());
        for (k, v) in [
            ("OPENCODE_DISABLE_CLAUDE_CODE", "1"),
            ("OPENCODE_DISABLE_EXTERNAL_SKILLS", "1"),
            ("OPENCODE_DISABLE_PROJECT_CONFIG", "1"),
            ("OPENCODE_DISABLE_MODELS_FETCH", "1"),
            ("OPENCODE_DISABLE_LSP_DOWNLOAD", "1"),
            ("OPENCODE_DISABLE_AUTOUPDATE", "1"),
            ("OPENCODE_DISABLE_SHARE", "1"),
            ("OPENCODE_DB", ":memory:"),
        ] {
            assert_eq!(
                inv.env
                    .iter()
                    .find(|(n, _)| n == k)
                    .map(|(_, v)| v.as_str()),
                Some(v),
                "{k}: hygiene, never isolation — it survives live mode"
            );
        }
    }

    /// The live route: the declaration is in the environment, whole and parseable, and it is the
    /// same declaration the file route writes.
    #[test]
    fn the_live_mcp_declaration_reaches_the_child_through_the_config_content_env_var() {
        let inv = compile_run(&live_spec());
        let (_, content) = inv
            .env
            .iter()
            .find(|(k, _)| k == CONFIG_CONTENT_ENV)
            .expect("a live node's ONLY bridge route");
        let v: Value = serde_json::from_str(content).expect("inline JSONC must parse as JSON");
        assert_eq!(v["mcp"]["marion"]["type"], json!("local"));
        assert_eq!(
            v["mcp"]["marion"]["command"],
            json!(["/bin/marion-supervisor", "mcp"])
        );
        assert_eq!(
            v["mcp"]["marion"]["environment"]["MARION_AGENT_ID"],
            json!("019f-child")
        );
        // The same block the file route writes for the same node: only the mode and the endpoint
        // differ, because a live node has neither marion's endpoint nor its auth to declare.
        let live_bridge = BridgeEnv {
            auth: Auth::Inherited,
            base_url: None,
            ..bridge()
        };
        assert_eq!(
            v["mcp"]["marion"],
            config_json(&cfg(), Some(&live_bridge))["mcp"]["marion"],
            "the declaration does not change because it travelled by env instead of by file"
        );
    }

    /// **Neither `provider` nor `small_model`, and that is the point of the route.** The inline text
    /// *merges over* the operator's own config (S13), so either key would shadow the real provider
    /// a live node is meant to authenticate against.
    #[test]
    fn the_live_config_shadows_none_of_the_operators_own_provider_settings() {
        let v = live_config_json(Some(&bridge()));
        for shadowing in ["provider", "small_model", "model"] {
            assert!(
                v.get(shadowing).is_none(),
                "{shadowing} would merge over the operator's own config and override it"
            );
        }
        assert_eq!(
            v.as_object().unwrap().keys().collect::<Vec<_>>(),
            vec!["mcp"],
            "marion's bridge and nothing else"
        );
        assert!(live_config_json(None).as_object().unwrap().is_empty());
    }

    #[test]
    fn an_absent_api_key_omits_the_field_rather_than_writing_an_empty_one() {
        let mut c = cfg();
        c.api_key = None;
        let v = config_json(&c, None);
        assert!(v["provider"]["canned"]["options"]["apiKey"].is_null());
    }
}
