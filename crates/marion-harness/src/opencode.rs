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

use marion_core::contract::AgentId;
use serde_json::{Value, json};

// The bridge's env contract, imported for the same reason gemini imports it: one spelling.
use crate::adapter::Auth;
use crate::claude_code::{
    AGENT_ID_ENV, AGENT_TYPE_ENV, AUTH_ENV, BASE_URL_ENV, DEPTH_ENV, READY_FILE_ENV,
};
use crate::invocation::Invocation;
use crate::stream::{StreamOutcome, first_string, json_frames};

/// The MCP server alias. opencode exposes MCP tools to the model as `<serverName>_<toolName>`, so
/// this alias is literally half of `marion_report`.
pub const MCP_ALIAS: &str = "marion";

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
    sandbox.join("config")
}

/// Where [`config_json`] is written: the first entry of opencode's merge order.
pub fn config_path(sandbox: &Path) -> PathBuf {
    xdg_config_home(sandbox)
        .join("opencode")
        .join("opencode.json")
}

/// The env that makes a child *this* node's child and nobody else's.
///
/// Every variable here was measured in S13, and two of them are not hygiene:
///
/// - **`OPENCODE_DISABLE_CLAUDE_CODE=1` / `OPENCODE_DISABLE_EXTERNAL_SKILLS=1`.** By default
///   opencode reads `~/.claude/CLAUDE.md`, **every** `CLAUDE.md` between cwd and the worktree
///   root, `~/.claude/skills/**/SKILL.md` and every project `.claude/skills/**/SKILL.md`. An
///   unsevered child therefore silently adopts a **different harness's** user configuration —
///   defeating `inherit_user_config: false` by a route that default never covered, and one no
///   amount of `XDG_*` isolation fixes, because `~/.claude` is found via `HOME`
///   (`tests/fixtures/s13/`).
/// - **`HOME` plus all four XDG vars.** There is no `CODEX_HOME` analogue: paths resolve through
///   `XDG_{CONFIG,DATA,CACHE,STATE}_HOME`, while `Path.home` independently drives the `~/.claude`,
///   `~/.agents` and `~/.opencode` lookups. Setting three of the five isolates nothing in
///   particular.
///
/// The remainder suppress work a spawned node has no business doing: a boot models.dev fetch plus
/// a 60-minute in-process loop, an LSP toolchain download, an auto-updater that pipes an install
/// script into `bash`, session sharing, a `.gitignore` written into every project config dir, and
/// an on-disk sqlite file.
///
/// **What this env cannot do is bound the run.** opencode *never exits* on a provider hang: S13
/// measured a 500 still retrying at 90 s and a connection-refused still hung at 180 s, with no
/// backoff ceiling found. `provider.options.timeout` / `headerTimeout` ([`config_json`]) are a
/// first line of defence, not a substitute — so marion's own bounded run and §9 two-step group
/// kill are **load-bearing for an opencode child, not defensive**.
pub fn isolation_env(sandbox: &Path) -> Vec<(String, String)> {
    let p = |sub: &str| sandbox.join(sub).to_string_lossy().into_owned();
    vec![
        ("HOME".into(), sandbox.to_string_lossy().into_owned()),
        (
            "XDG_CONFIG_HOME".into(),
            xdg_config_home(sandbox).to_string_lossy().into_owned(),
        ),
        ("XDG_DATA_HOME".into(), p("data")),
        ("XDG_CACHE_HOME".into(), p("cache")),
        ("XDG_STATE_HOME".into(), p("state")),
        ("OPENCODE_DISABLE_CLAUDE_CODE".into(), "1".into()),
        ("OPENCODE_DISABLE_EXTERNAL_SKILLS".into(), "1".into()),
        ("OPENCODE_DISABLE_PROJECT_CONFIG".into(), "1".into()),
        ("OPENCODE_DISABLE_MODELS_FETCH".into(), "1".into()),
        ("OPENCODE_DISABLE_LSP_DOWNLOAD".into(), "1".into()),
        ("OPENCODE_DISABLE_AUTOUPDATE".into(), "1".into()),
        ("OPENCODE_DISABLE_SHARE".into(), "1".into()),
        ("OPENCODE_DB".into(), ":memory:".into()),
    ]
}

/// What marion needs to compile an `opencode run` invocation.
#[derive(Debug, Clone)]
pub struct RunSpec {
    pub cwd: PathBuf,
    /// The isolated home: `HOME` and the parent of all four XDG roots.
    pub sandbox: PathBuf,
    pub model: ModelRef,
    /// **Required.** Without `--title` opencode issues an extra `You are a title generator`
    /// request against `small_model` — S13 measured **3 POSTs instead of 2**. Same shape as
    /// §5.5's Claude Code session-title request.
    pub title: String,
    pub prompt: String,
}

pub fn compile_run(spec: &RunSpec) -> Invocation {
    let args: Vec<String> = vec![
        "run".into(),
        // Skips loading external plugins. It does *not* gate the forkDetach'ed
        // `@opencode-ai/plugin` npm install or the ripgrep auto-download (S13) — those are bounded
        // by the throwaway HOME above, not by a flag.
        "--pure".into(),
        "--format".into(),
        "json".into(),
        "--title".into(),
        spec.title.clone(),
        "-m".into(),
        spec.model.qualified(),
        // The prompt is **positional** (variadic). stdin works identically, but a LaunchOnly
        // surface gives the child no stdin.
        spec.prompt.clone(),
    ];

    Invocation {
        program: "opencode".into(),
        args,
        env: isolation_env(&spec.sandbox),
        cwd: spec.cwd.clone(),
        // The `provider/model` pair `-m` carries, which is also the config's `model` key.
        model: Some(spec.model.qualified()),
    }
}

/// Parse an `opencode run --pure --format json` stream.
///
/// The event set is closed and measured (`tests/fixtures/s13/`): `step_start | step_finish | text |
/// reasoning | tool_use | error`. Every line carries `{type, timestamp, sessionID, …}`.
///
/// **There is no terminal frame, and this parser must never wait for one.** S13: *"There is no
/// init, result or usage summary event. The stream simply ends when `session.status === "idle"`. A
/// reader must terminate on stdout close, not on a terminal frame — the contrast with codex and
/// gemini, which both emit one."* That property is honoured in two places and both matter: this
/// function is a fold over however many frames arrived and has no notion of a last one, and
/// [`crate::stream::FrameSplitter::finish`] delivers a trailing unterminated frame at close. The
/// supervisor's own drain already ends on EOF, so nothing upstream waits either.
///
/// The two evidence-bearing shapes, verbatim from S13:
///
/// - `tool_use` fires **only on terminal states** (`completed` / `error`) — there are no streaming
///   partials — and carries the call's `input` under `part.state.input`. So a report's narrative is
///   read there, from the call marion's own bridge was handed;
/// - `error` carries `{name, data:{message, statusCode, …}}`, which S13 measured arriving with
///   **exit 1 and an empty stderr**, so the stream is the only place that failure is described.
///
/// `file_change_paths` stays empty: opencode announces no file-change event at all, and git is the
/// authority for `changed_paths` regardless.
pub fn parse_stream(s: &str, report_tool: &str) -> StreamOutcome {
    let mut out = StreamOutcome::default();
    for v in json_frames(s) {
        match v["type"].as_str() {
            Some("tool_use") if v["part"]["tool"].as_str() == Some(report_tool) => {
                let state = &v["part"]["state"];
                match state["status"].as_str() {
                    Some("completed") => {
                        if let Some(n) = state["input"]["narrative"].as_str() {
                            out.narrative = Some(n.to_string());
                        }
                    }
                    // The measured rejection shape: `{"status":"error","error":"The user rejected
                    // permission…"}`. The run continues and exits 0, so without this the contract
                    // would record a clean run in which marion's tool was refused.
                    Some("error") => {
                        out.failure = Some(format!(
                            "the child's {report_tool} call ended in error: {}",
                            state["error"].as_str().unwrap_or("no message")
                        ));
                    }
                    _ => {}
                }
            }
            Some("error") => {
                out.failure = out.failure.take().or_else(|| {
                    first_string(&v, &["/error/data/message", "/error/name"])
                        .or_else(|| Some("the child's stream carried an error frame".into()))
                });
            }
            _ => {}
        }
    }
    out
}

/// Every marion tool this stream shows the node calling, in **marion's** vocabulary.
///
/// The tool's name sits under `part.tool` — a third field in a third place — and every state is
/// counted, including `error`: the question this answers is "did the node reach marion's bridge",
/// and a call the bridge refused reached it just as surely as one it served.
pub fn marion_tool_calls(s: &str, prefix: &str) -> Vec<String> {
    json_frames(s)
        .iter()
        .filter(|v| v["type"].as_str() == Some("tool_use"))
        .filter_map(|v| {
            v["part"]["tool"]
                .as_str()
                .and_then(|n| n.strip_prefix(prefix))
                .map(str::to_string)
        })
        .collect()
}

/// The values [`config_json`] writes into the MCP declaration.
#[derive(Debug, Clone)]
pub struct BridgeEnv {
    pub bridge: PathBuf,
    pub args: Vec<String>,
    pub repo: PathBuf,
    pub state: PathBuf,
    /// `None` under [`Auth::Inherited`]: the key is omitted, never written empty
    /// ([`crate::claude_code::BASE_URL_ENV`]).
    pub base_url: Option<String>,
    /// Which endpoint a child this node spawns should talk to
    /// ([`crate::claude_code::AUTH_ENV`]).
    pub auth: Auth,
    pub agent_id: AgentId,
    /// The node's canonical agent type name (§6.1 step 2 reads the caller's type).
    pub agent_type: String,
    /// The node's depth, root = 0 (§3.1's `max_depth`).
    pub depth: u32,
    /// `None` on this surface — the prompt rides argv (§6.1 step 8).
    pub ready_file: Option<PathBuf>,
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
        let mut environment = json!({
            "MARION_REPO": b.repo.to_string_lossy(),
            "MARION_STATE_DIR": b.state.to_string_lossy(),
            AUTH_ENV: b.auth.as_wire(),
            AGENT_ID_ENV: b.agent_id.0,
            AGENT_TYPE_ENV: b.agent_type,
            DEPTH_ENV: b.depth.to_string(),
        });
        if let Some(u) = &b.base_url {
            environment[BASE_URL_ENV] = json!(u);
        }
        if let Some(r) = &b.ready_file {
            environment[READY_FILE_ENV] = json!(r.to_string_lossy());
        }
        let mut command: Vec<String> = vec![b.bridge.to_string_lossy().into_owned()];
        command.extend(b.args.iter().cloned());
        config["mcp"] = json!({
            MCP_ALIAS: {
                "type": "local",
                "command": command,
                "environment": environment,
                "enabled": true,
            }
        });
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> ModelRef {
        ModelRef::parse("canned/canned-1").unwrap()
    }

    fn spec() -> RunSpec {
        RunSpec {
            cwd: "/tmp/wt".into(),
            sandbox: "/tmp/sb".into(),
            model: model(),
            title: "marion-019f-child".into(),
            prompt: "do the task".into(),
        }
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

    #[test]
    fn an_absent_api_key_omits_the_field_rather_than_writing_an_empty_one() {
        let mut c = cfg();
        c.api_key = None;
        let v = config_json(&c, None);
        assert!(v["provider"]["canned"]["options"]["apiKey"].is_null());
    }
}
