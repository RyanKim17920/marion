//! The GitHub Copilot CLI adapter — the headless `-p` surface (fixture `tests/fixtures/s24/`).
//!
//! Measured against **copilot 1.0.83** on 2026-09-05, every probe against a canned local OpenAI
//! Chat Completions endpoint at $0.00. The shape is `codex exec`'s and gemini's: marion compiles
//! argv + env, writes one document, starts the process, and reads its JSONL. There is no channel
//! to steer a turn, so control is `LaunchOnly` and the prompt rides argv.
//!
//! What is *not* like the other four, and is the content of this module:
//!
//! - **Both of §3.1's axes exist, as two different flags with two different spellings.**
//!   `--available-tools` decides what the model *sees* (the availability axis; everything it does
//!   not name is withheld, and the CLI says so in a `session.info` frame), and `--allow-tool`
//!   decides what runs *without a prompt* (the permission axis). In `-p` mode there is nobody to
//!   prompt, so a visible tool with no grant is answered `Permission denied and could not request
//!   permission from user` — **and the run exits 0.** Both axes are therefore marion's to set, as
//!   on Claude Code, and opening only the first is the same measured dead end §11 item 24 records
//!   there (`copilot-create-denied-without-grant.stdout.jsonl`).
//! - **The two axes do not spell a tool the same way.** The model-facing name of an MCP tool is
//!   `<server>-<tool>` (`marion-report`, a fifth spelling after four); the permission pattern for
//!   the same tool is `<server>(<tool>)` (`marion(report)`), and the pattern for the built-in
//!   file tools is the kind `write`, not a tool name. [`permission_pattern`] holds the rule.
//! - **BYOK is env, not config.** `COPILOT_PROVIDER_BASE_URL` selects a provider outright, and
//!   with it GitHub authentication is not required. A model is mandatory under BYOK: without one
//!   the CLI exits 1 with `BYOK providers require an explicit model` before any request.
//! - **The API key is redacted from the JSONL.** Every occurrence of the
//!   `COPILOT_PROVIDER_API_KEY` string in the CLI's own output is replaced with `******`, so a
//!   credential whose text appears in a narrative or a model name corrupts what marion reads.
//!   marion's canned placeholder does not appear in anything a child prints; a live key is the
//!   operator's own and is never placed by marion.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use marion_core::contract::AgentId;
use marion_core::harness::Harness;
use serde_json::{Value, json};

use crate::auth::Auth;
// The **bridge's** env contract, imported rather than respelled for the reason gemini and opencode
// give: the bridge reads `MARION_AGENT_ID` whichever harness started it.
use crate::mcp_bridge::{
    AGENT_ID_ENV, AGENT_TYPE_ENV, AUTH_ENV, BASE_URL_ENV, DEPTH_ENV, NODE_TOKEN_ENV, READY_FILE_ENV,
};
use crate::spec::{Arg, Env, Field, HarnessSpec, Val, When};
use crate::stream::{
    CallOutcome, MarionCall, StreamOutcome, first_string, json_frames, report_commits,
};

/// `$COPILOT_HOME`'s name under the node's config dir — one spelling for [`SPEC`]'s env row and
/// [`home`].
const HOME_DIR: &str = "home";

/// Copilot's row. Measured against 1.0.83 on 2026-09-05 (`tests/fixtures/s24/`), every probe
/// against a canned local OpenAI Chat Completions endpoint at $0.00.
///
/// **Live mode is a removal.** The home relocation and the whole provider block go together: they
/// exist to point a node at marion's endpoint without touching `~/.copilot`, and a live node's
/// premise is the opposite — the operator's own login, from the operator's own home. The one
/// survivor is the auto-update switch, which was never isolation.
pub const SPEC: HarnessSpec = HarnessSpec {
    harness: Harness::Copilot,
    program: Some("copilot"),
    argv: &[
        // The prompt is the **argument to `-p`**: `-p` is what selects non-interactive mode at
        // all, and a bare positional would open the TUI.
        Arg::Flag("-p", Field::Prompt),
        Arg::Lit("--output-format"),
        Arg::Lit("json"),
        // `-C` places the node **and** bounds the built-in file tools: the permission help says
        // file access is restricted to the working directory and its subdirectories by default,
        // so a worktree is the whole of what `create`/`view` can reach.
        Arg::Flag("-C", Field::Cwd),
        // The GitHub MCP server, which is a network round trip on a live node and dead weight on
        // a canned one; `--available-tools` below would hide its tools anyway.
        Arg::Lit("--disable-builtin-mcps"),
        // `AGENTS.md`, `.github/copilot-instructions.md` and friends out of the operator's repo.
        // marion's prompt is the whole of the node's instructions (§3.1).
        Arg::Lit("--no-custom-instructions"),
        Arg::Flag("--model", Field::Model),
        // `=`-joined on both axes: the flags are variadic (`[=tools...]`), and a space-separated
        // value would leave the parser free to swallow whatever came next. An empty availability
        // list compiles **no flag**: `--available-tools=` with nothing after it was measured to
        // disable nothing, so emitting it would claim a constraint that does not exist.
        Arg::FlagEq("--available-tools", Field::Tools),
        Arg::EachEq("--allow-tool", Field::Allowed),
        Arg::Flag("--additional-mcp-config", Field::McpConfig),
    ],
    pane: None,
    env: &[
        Env {
            key: HOME_ENV,
            val: Val::Under(HOME_DIR),
            when: When::Canned,
        },
        Env {
            key: PROVIDER_BASE_URL_ENV,
            val: Val::Field(Field::BaseUrl),
            when: When::Canned,
        },
        Env {
            key: PROVIDER_TYPE_ENV,
            val: Val::Lit(PROVIDER_TYPE),
            when: When::Canned,
        },
        Env {
            key: PROVIDER_WIRE_API_ENV,
            val: Val::Lit(WIRE_API),
            when: When::Canned,
        },
        Env {
            key: PROVIDER_API_KEY_ENV,
            val: Val::Field(Field::ApiKey),
            when: When::Canned,
        },
        Env {
            key: OFFLINE_ENV,
            val: Val::Lit("true"),
            when: When::Canned,
        },
        Env {
            key: AUTO_UPDATE_ENV,
            val: Val::Lit("false"),
            when: When::Always,
        },
    ],
    note: "s24 on copilot 1.0.83: the -p surface, BYOK by env, both tool axes in their two \
           spellings, the @-file declaration route; harness_matrix's copilot cell runs this row \
           end to end",
};

/// Relocates configuration and state — `config.json`, `session-state/`, `logs/`,
/// `session-store.db`, `mcp-config.json`, `installed-plugins/`. The CLI's default is
/// `$HOME/.copilot`, and the GitHub login lives there too, which is why this is **dropped under
/// [`Auth::Inherited`]**: relocating it is exactly what hides the operator's own credential from a
/// node meant to present it. `HOME` itself is left alone under both modes — the 1.0.83 launcher
/// resolves its platform package through `~/Library/Caches/copilot/pkg/`, and a relocated `HOME`
/// would send every child looking for a binary that is not there.
pub const HOME_ENV: &str = "COPILOT_HOME";
/// Selects a custom provider (BYOK) outright; with it set, GitHub authentication is not required.
/// Takes marion's canonical `…/v1` form **verbatim**: the CLI appends `/chat/completions` to it
/// (measured: `POST /v1/chat/completions` against a `…/v1` base). **Dropped under
/// [`Auth::Inherited`]**: live mode names no endpoint.
pub const PROVIDER_BASE_URL_ENV: &str = "COPILOT_PROVIDER_BASE_URL";
/// `openai` | `azure` | `anthropic`. marion states [`PROVIDER_TYPE`] explicitly even though it is
/// the CLI's default, because a default is a fact about a version and this is a fact about marion.
pub const PROVIDER_TYPE_ENV: &str = "COPILOT_PROVIDER_TYPE";
/// `completions` | `responses`. Stated for the same reason.
pub const PROVIDER_WIRE_API_ENV: &str = "COPILOT_PROVIDER_WIRE_API";
/// Sent as `Authorization: Bearer …` and **redacted from the CLI's own JSONL wherever the string
/// appears** (see the module docs). **Dropped under [`Auth::Inherited`]**.
pub const PROVIDER_API_KEY_ENV: &str = "COPILOT_PROVIDER_API_KEY";
/// `true` skips every network path but the provider: GitHub auth, telemetry, web tools, the GitHub
/// MCP server and auto-update. Requires [`PROVIDER_BASE_URL_ENV`], so it travels with it and is
/// **dropped under [`Auth::Inherited`]**, where GitHub auth is the whole point.
pub const OFFLINE_ENV: &str = "COPILOT_OFFLINE";
/// `false` disables the CLI downloading a newer build of itself on start-up — the §7.7 hazard that
/// moved codex and claude under the suite three times in five days. Kept under **both** modes: it
/// is hygiene, not isolation.
pub const AUTO_UPDATE_ENV: &str = "COPILOT_AUTO_UPDATE";

/// The provider type marion compiles: marion's canned server speaks OpenAI Chat Completions, which
/// is the `openai` type on its default `completions` wire. **Not** `anthropic`, although the
/// canned server speaks that too and the CLI offers it: the opencode row already proves this
/// endpoint's Chat Completions SSE end to end, so this is the wire with the most evidence behind
/// it, and a harness that can choose either should choose the one already measured.
pub const PROVIDER_TYPE: &str = "openai";
/// See [`PROVIDER_TYPE`].
pub const WIRE_API: &str = "completions";

/// The MCP server alias. The model-facing tool name is `<alias>-<tool>` and the permission
/// pattern is `<alias>(<tool>)`, so this string is literally half of both `marion-report` and
/// `marion(report)`.
pub const MCP_ALIAS: &str = "marion";

/// The `--allow-tool` **kind** that grants the built-in file tools — *"tools that create and
/// modify files, except shell tool invocations"* (`copilot help permissions`). A kind, not a tool
/// name: `--allow-tool=create` was measured to grant nothing (the call was still `denied`), and
/// `--allow-tool=write` to grant `create` (`copilot-write-then-report.stdout.jsonl`, where the
/// `session.info` `file_created` frame is the file landing).
pub const WRITE_PERMISSION: &str = "write";

/// Is this copilot-native tool name one [`WRITE_PERMISSION`] is required for?
///
/// The two built-ins whose calls the permission help files under `write(path?)`. `bash` can write
/// too, through redirection, and is deliberately **not** here: marion's vocabulary has no verb
/// that means it, and the help says shell redirections need `--allow-all-tools`, which marion
/// never compiles.
pub fn is_write_tool(native: &str) -> bool {
    matches!(native, "create" | "edit")
}

/// The model-facing spelling of one of marion's tools: `<alias>-<tool>`. Measured in the request
/// body's `tools[].function.name` and in the stream's `toolName`
/// (`copilot-write-then-report.provider-request-1.json`).
pub fn model_tool_name(alias: &str, tool: &str) -> String {
    format!("{alias}-{tool}")
}

/// The `--allow-tool` pattern for one of marion's tools: `<alias>(<tool>)`, the
/// `<mcp-server-name>(tool-name?)` kind from `copilot help permissions`.
///
/// **The model-facing name is not a pattern.** `--allow-tool=marion-report` was measured to grant
/// nothing — the call was `denied` exactly as with no flag at all — so an adapter that reused the
/// spelling it compiles into `--available-tools` here would open the availability axis and leave
/// the permission axis shut, silently. See the module docs.
pub fn permission_pattern(alias: &str, tool: &str) -> String {
    format!("{alias}({tool})")
}

/// `$COPILOT_HOME` for a node: a directory under marion's own config dir, never `~/.copilot`.
pub fn home(config_dir: &Path) -> PathBuf {
    config_dir.join(HOME_DIR)
}

/// Where [`mcp_config_json`] is written, named by `--additional-mcp-config @<path>`.
pub fn mcp_config_path(config_dir: &Path) -> PathBuf {
    config_dir.join("mcp.json")
}

/// The values [`mcp_config_json`] writes into the MCP server declaration.
#[derive(Debug, Clone)]
pub struct BridgeEnv {
    pub bridge: PathBuf,
    pub args: Vec<String>,
    pub repo: PathBuf,
    pub state: PathBuf,
    /// `None` under [`Auth::Inherited`]: the key is omitted, never written empty
    /// ([`crate::mcp_bridge::BASE_URL_ENV`]).
    pub base_url: Option<String>,
    pub auth: Auth,
    pub agent_id: AgentId,
    pub agent_type: String,
    pub depth: u32,
    /// Present or absent, never empty ([`crate::mcp_bridge::NODE_TOKEN_ENV`]).
    pub node_token: Option<String>,
    /// `None` on this surface: the prompt rides argv, so there is no first frame to withhold.
    ///
    /// **And copilot needs none.** Measured: the CLI holds turn one until every declared MCP
    /// server has finished `initialize` + `tools/list` (`session.mcp_servers_loaded` precedes
    /// `user.message` in every capture), with a 60 s ceiling after which it proceeds tool-less and
    /// says so in that frame (`status: "failed"`). That is the gate §6.1 step 8 makes marion
    /// build for Claude Code, built into the harness.
    pub ready_file: Option<PathBuf>,
}

/// The `--additional-mcp-config` document: the same shape as `~/.copilot/mcp-config.json`, which
/// it *augments* for one session rather than replacing.
///
/// `"tools": ["*"]` is load-bearing: it is the server's tool allowlist, and the key is required —
/// without it the server's tools are not exposed to the model. `type: "stdio"` with `command` as a
/// string and `args` as an array is the shape the CLI's own `copilot mcp add` writes.
pub fn mcp_config_json(b: &BridgeEnv) -> Value {
    let mut env = json!({
        "MARION_REPO": b.repo.to_string_lossy(),
        "MARION_STATE_DIR": b.state.to_string_lossy(),
        AUTH_ENV: b.auth.as_wire(),
        AGENT_ID_ENV: b.agent_id.0,
        AGENT_TYPE_ENV: b.agent_type,
        DEPTH_ENV: b.depth.to_string(),
    });
    if let Some(u) = &b.base_url {
        env[BASE_URL_ENV] = json!(u);
    }
    if let Some(t) = &b.node_token {
        env[NODE_TOKEN_ENV] = json!(t);
    }
    if let Some(r) = &b.ready_file {
        env[READY_FILE_ENV] = json!(r.to_string_lossy());
    }
    json!({
        "mcpServers": {
            MCP_ALIAS: {
                "type": "stdio",
                "command": b.bridge.to_string_lossy(),
                "args": b.args,
                "env": env,
                "tools": ["*"],
            }
        }
    })
}

/// What one `tool.execution_complete` frame said, keyed by `toolCallId`.
///
/// `success: true` is the only answered shape measured; `success: false` always carried
/// `error.message` and an `error.code` — `"denied"` for a call with no grant, `"failure"` for an
/// `isError` MCP result and for a built-in that failed on its own terms.
fn completions(frames: &[Value]) -> BTreeMap<String, CallOutcome> {
    let mut out = BTreeMap::new();
    for v in frames {
        if v["type"].as_str() != Some("tool.execution_complete") {
            continue;
        }
        let Some(id) = v["data"]["toolCallId"].as_str() else {
            continue;
        };
        let outcome = match v["data"]["success"].as_bool() {
            Some(true) => CallOutcome::Answered,
            Some(false) => CallOutcome::Refused(
                first_string(v, &["/data/error/message", "/data/error/code"])
                    .unwrap_or_else(|| "the tool call failed without a message".into()),
            ),
            None => CallOutcome::Unknown,
        };
        out.insert(id.to_string(), outcome);
    }
    out
}

/// Parse a `copilot -p … --output-format json` stream.
///
/// The event set (`tests/fixtures/s24/`): `session.*`, `user.message`, `assistant.turn_start`,
/// `model.call_start` / `model.call_finished` / `model.call_failure`, `assistant.message_start` /
/// `assistant.message_delta` / `assistant.message`, `assistant.tool_call_delta`,
/// `tool.execution_start` / `tool.execution_complete`, `assistant.turn_end`, `assistant.idle`,
/// `session.error`, `result`. Every frame but `result` is `{type, data, id, timestamp, parentId}`;
/// `result` carries `exitCode` and `usage` at the top level.
///
/// **A report is read off `tool.execution_start`, and its verdict off `tool.execution_complete`,
/// paired by `toolCallId`.** The start frame is the one that says the harness dispatched the call
/// (the `assistant.message.toolRequests[]` entry before it is only the model asking), and its
/// `arguments` are the parsed object rather than the `inputDelta` fragments — the capture shows a
/// path split mid-string across two deltas, which is why the deltas are not read.
///
/// **The exit code is not the verdict, and that is measured.** A `report` denied for want of a
/// grant, and a `report` the bridge answered `isError: true`, both end the run at `exitCode: 0`
/// with the model's closing text intact — §12's silent-success shape — so a failed report is a
/// failure here whatever the process exited with. `session.error` is the CLI's own failure claim
/// (measured on a provider 500 after five retries, beside `exitCode: 1`), and a non-zero
/// `result.exitCode` is recorded when nothing more specific was.
pub fn parse_stream(s: &str, report_tool: &str) -> StreamOutcome {
    let frames = json_frames(s);
    let verdicts = completions(&frames);
    let mut out = StreamOutcome::default();
    for v in &frames {
        match v["type"].as_str() {
            Some("tool.execution_start") if v["data"]["toolName"].as_str() == Some(report_tool) => {
                let args = &v["data"]["arguments"];
                if let Some(n) = args["narrative"].as_str() {
                    out.narrative = Some(n.to_string());
                }
                out.result_commits = report_commits(args);
                if let Some(CallOutcome::Refused(why)) = v["data"]["toolCallId"]
                    .as_str()
                    .and_then(|id| verdicts.get(id))
                {
                    out.failure = Some(format!(
                        "the child's {report_tool} call ended in error: {why}"
                    ));
                }
            }
            Some("session.error") => {
                out.failure = out.failure.take().or_else(|| {
                    first_string(v, &["/data/message", "/data/errorType"])
                        .or_else(|| Some("the child's stream carried a session.error frame".into()))
                });
            }
            Some("result") => {
                if let Some(code) = v["exitCode"].as_i64()
                    && code != 0
                {
                    out.failure = out.failure.take().or_else(|| {
                        Some(format!("copilot's result frame reported exitCode {code}"))
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// Every marion tool this stream shows the node calling, in **marion's** vocabulary, with what came
/// of each call.
///
/// The call is `tool.execution_start`'s `data.toolName`, in the `<alias>-` spelling, and the
/// verdict is the `tool.execution_complete` with the same `data.toolCallId`. A start with no
/// completion is [`CallOutcome::Unknown`] — a run killed mid-call — and never an answer.
pub fn marion_calls(s: &str, prefix: &str) -> Vec<MarionCall> {
    let frames = json_frames(s);
    let verdicts = completions(&frames);
    frames
        .iter()
        .filter(|v| v["type"].as_str() == Some("tool.execution_start"))
        .filter_map(|v| {
            let verb = v["data"]["toolName"]
                .as_str()?
                .strip_prefix(prefix)?
                .to_string();
            let outcome = v["data"]["toolCallId"]
                .as_str()
                .and_then(|id| verdicts.get(id).cloned())
                .unwrap_or(CallOutcome::Unknown);
            Some(MarionCall { verb, outcome })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four s24 captures this module's claims rest on, verbatim (paths and ids redacted).
    const WRITE_THEN_REPORT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s24/copilot-write-then-report.stdout.jsonl"
    ));
    const CREATE_DENIED: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s24/copilot-create-denied-without-grant.stdout.jsonl"
    ));
    const REPORT_ISERROR: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s24/copilot-report-iserror.stdout.jsonl"
    ));
    const PROVIDER_500: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s24/copilot-provider-500.stdout.jsonl"
    ));

    use crate::adapter::{
        CopilotAdapter, Extras, HarnessAdapter, LaunchSpec, McpDeclaration, SpawnCtx,
    };
    use crate::invocation::Invocation;
    use marion_core::agent_type;

    fn ctx() -> SpawnCtx {
        SpawnCtx {
            agent_id: AgentId("019f-child".into()),
            agent_type: "copilot".into(),
            depth: 1,
            node_token: None,
            ready_file: None,
            repo: "/repo".into(),
            state_dir: "/state".into(),
            bridge: "/bin/marion-supervisor".into(),
            bridge_args: vec!["mcp".into()],
        }
    }

    /// `allowed_tools` arrives in **this harness's** model-facing spelling, as `run_spawn` hands
    /// it; the adapter turns it into both axes.
    fn spec() -> LaunchSpec {
        LaunchSpec {
            cwd: "/tmp/wt".into(),
            model: Some("canned-1".into()),
            prompt: "do the task".into(),
            tools: vec![],
            allowed_tools: vec!["marion-report".into()],
            mcp: McpDeclaration::Marion,
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            api_key: Some("sk-fake".into()),
            auth: Auth::Canned,
            config_dir: "/tmp/cfg".into(),
            extra: Extras::default(),
        }
    }

    fn live_spec() -> LaunchSpec {
        LaunchSpec {
            base_url: None,
            api_key: None,
            auth: Auth::Inherited,
            ..spec()
        }
    }

    fn compile_prompt(spec: &LaunchSpec) -> Invocation {
        CopilotAdapter.compile(spec, &ctx()).unwrap()
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
            agent_type: "copilot".into(),
            depth: 1,
            node_token: None,
            ready_file: None,
        }
    }

    fn env_of(inv: &Invocation, k: &str) -> Option<String> {
        inv.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn the_prompt_is_the_argument_to_p_and_the_output_is_jsonl() {
        let inv = compile_prompt(&spec());
        assert_eq!(inv.program, "copilot");
        assert_eq!(&inv.args[..2], ["-p", "do the task"]);
        let f = inv
            .args
            .iter()
            .position(|a| a == "--output-format")
            .unwrap();
        assert_eq!(inv.args[f + 1], "json");
        let c = inv.args.iter().position(|a| a == "-C").unwrap();
        assert_eq!(inv.args[c + 1], "/tmp/wt");
        assert_eq!(inv.cwd, PathBuf::from("/tmp/wt"));
    }

    /// Measured: `--allow-tool=marion-report` grants nothing, `--allow-tool=marion(report)` grants
    /// the call. The two axes take two spellings and the argv must carry each in its own.
    #[test]
    fn the_two_axes_are_compiled_as_two_flags_in_two_spellings() {
        // A declared `write` is `create` on the availability axis and the kind `write` on the
        // permission axis; marion's own verb is `marion-report` on one and `marion(report)` on
        // the other.
        let inv = compile_prompt(&LaunchSpec {
            tools: vec![agent_type::TOOL_WRITE.into()],
            ..spec()
        });
        assert!(
            inv.args
                .contains(&"--available-tools=marion-report,create".to_string()),
            "{:?}",
            inv.args
        );
        assert!(
            inv.args
                .contains(&"--allow-tool=marion(report)".to_string())
        );
        assert!(inv.args.contains(&"--allow-tool=write".to_string()));
        assert!(
            !inv.args.iter().any(|a| a == "--allow-tool=marion-report"),
            "the model-facing spelling is not a permission pattern and grants nothing"
        );
        assert!(
            !inv.args.iter().any(|a| a.starts_with("--allow-all")),
            "marion never compiles the blanket grant"
        );
    }

    /// `--available-tools=` with nothing after it disables nothing (measured), so an empty list
    /// compiles no flag rather than a flag that claims a constraint.
    #[test]
    fn an_empty_availability_list_compiles_no_flag_rather_than_an_empty_one() {
        let inv = compile_prompt(&LaunchSpec {
            tools: vec![],
            allowed_tools: vec![],
            ..spec()
        });
        assert!(
            !inv.args.iter().any(|a| a.starts_with("--available-tools")),
            "{:?}",
            inv.args
        );
        assert!(!inv.args.iter().any(|a| a.starts_with("--allow-tool")));
    }

    #[test]
    fn the_mcp_document_is_named_with_the_at_prefix_the_flag_reads_a_file_through() {
        let inv = compile_prompt(&spec());
        let i = inv
            .args
            .iter()
            .position(|a| a == "--additional-mcp-config")
            .unwrap();
        assert_eq!(inv.args[i + 1], "@/tmp/cfg/mcp.json");
        let none = compile_prompt(&LaunchSpec {
            mcp: McpDeclaration::None,
            ..spec()
        });
        assert!(!none.args.iter().any(|a| a == "--additional-mcp-config"));
    }

    #[test]
    fn the_builtin_mcp_and_the_repos_own_instructions_are_kept_out() {
        let inv = compile_prompt(&spec());
        assert!(inv.args.contains(&"--disable-builtin-mcps".to_string()));
        assert!(inv.args.contains(&"--no-custom-instructions".to_string()));
    }

    /// The whole BYOK block, and the relocated home, under canned; the base URL verbatim in its
    /// `…/v1` form because the CLI appends `/chat/completions` itself.
    #[test]
    fn a_canned_node_is_pointed_at_marions_endpoint_by_env_and_relocated() {
        let inv = compile_prompt(&spec());
        assert_eq!(env_of(&inv, HOME_ENV).as_deref(), Some("/tmp/cfg/home"));
        assert_eq!(
            env_of(&inv, PROVIDER_BASE_URL_ENV).as_deref(),
            Some("http://127.0.0.1:8099/v1")
        );
        assert_eq!(env_of(&inv, PROVIDER_TYPE_ENV).as_deref(), Some("openai"));
        assert_eq!(
            env_of(&inv, PROVIDER_WIRE_API_ENV).as_deref(),
            Some("completions")
        );
        assert_eq!(
            env_of(&inv, PROVIDER_API_KEY_ENV).as_deref(),
            Some("sk-fake")
        );
        assert_eq!(env_of(&inv, OFFLINE_ENV).as_deref(), Some("true"));
        assert_eq!(env_of(&inv, AUTO_UPDATE_ENV).as_deref(), Some("false"));
        assert!(
            env_of(&inv, "HOME").is_none(),
            "HOME stays: the launcher resolves its platform package under ~/Library/Caches"
        );
    }

    /// Live mode is a removal: the home stays the operator's, no provider is named, and the one
    /// survivor is the auto-update switch, which was never isolation.
    #[test]
    fn a_live_node_drops_the_home_relocation_and_the_whole_provider_block() {
        let inv = compile_prompt(&live_spec());
        for k in [
            HOME_ENV,
            PROVIDER_BASE_URL_ENV,
            PROVIDER_TYPE_ENV,
            PROVIDER_WIRE_API_ENV,
            PROVIDER_API_KEY_ENV,
            OFFLINE_ENV,
        ] {
            assert!(
                env_of(&inv, k).is_none(),
                "{k} must be dropped under --live"
            );
        }
        assert_eq!(env_of(&inv, AUTO_UPDATE_ENV).as_deref(), Some("false"));
        // And the argv is the same argv: the axes and the MCP route do not depend on auth.
        assert_eq!(inv.args, compile_prompt(&spec()).args);
    }

    /// Gated on the mode, not only on the value: a live spec handed an endpoint pushes none.
    #[test]
    fn a_live_spec_that_carries_an_endpoint_still_pushes_none() {
        let inv = compile_prompt(&LaunchSpec {
            auth: Auth::Inherited,
            ..spec()
        });
        assert!(env_of(&inv, PROVIDER_BASE_URL_ENV).is_none());
        assert!(env_of(&inv, PROVIDER_API_KEY_ENV).is_none());
        assert!(env_of(&inv, HOME_ENV).is_none());
    }

    #[test]
    fn the_model_is_carried_by_the_flag_and_recorded_from_it() {
        let inv = compile_prompt(&spec());
        let i = inv.args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(inv.args[i + 1], "canned-1");
        assert_eq!(inv.model.as_deref(), Some("canned-1"));
        let none = compile_prompt(&LaunchSpec {
            model: None,
            ..live_spec()
        });
        assert!(!none.args.iter().any(|a| a == "--model"));
        assert_eq!(none.model, None);
    }

    #[test]
    fn the_two_spellings_of_one_tool() {
        assert_eq!(model_tool_name(MCP_ALIAS, "report"), "marion-report");
        assert_eq!(permission_pattern(MCP_ALIAS, "report"), "marion(report)");
        assert!(is_write_tool("create"));
        assert!(is_write_tool("edit"));
        assert!(!is_write_tool("view"));
        assert!(
            !is_write_tool("bash"),
            "shell redirection is not a grant marion makes"
        );
    }

    /// The document `--additional-mcp-config` reads: the CLI's own `mcp-config.json` shape, with
    /// the bridge's identity in `env` and `tools: ["*"]` exposing the server's tools.
    #[test]
    fn the_mcp_document_carries_the_nodes_identity_in_the_clis_own_shape() {
        let mut b = bridge();
        b.node_token = Some("tok".into());
        let v = mcp_config_json(&b);
        let m = &v["mcpServers"]["marion"];
        assert_eq!(m["type"], json!("stdio"));
        assert_eq!(m["command"], json!("/bin/marion-supervisor"));
        assert_eq!(m["args"], json!(["mcp"]));
        assert_eq!(m["tools"], json!(["*"]));
        let e = &m["env"];
        assert_eq!(e["MARION_AGENT_ID"], json!("019f-child"));
        assert_eq!(e["MARION_AGENT_TYPE"], json!("copilot"));
        assert_eq!(e["MARION_DEPTH"], json!("1"));
        assert_eq!(e["MARION_REPO"], json!("/repo"));
        assert_eq!(e["MARION_STATE_DIR"], json!("/state"));
        assert_eq!(e["MARION_BASE_URL"], json!("http://127.0.0.1:8099/v1"));
        assert_eq!(e["MARION_NODE_TOKEN"], json!("tok"));
        assert!(
            e.get("MARION_READY_FILE").is_none(),
            "no marker on this surface: the CLI holds turn one for its MCP servers itself"
        );
    }

    #[test]
    fn an_absent_base_url_or_token_is_omitted_not_written_empty() {
        let mut b = bridge();
        b.base_url = None;
        let e = &mcp_config_json(&b)["mcpServers"]["marion"]["env"];
        assert!(e.get("MARION_BASE_URL").is_none());
        assert!(e.get("MARION_NODE_TOKEN").is_none());
    }

    #[test]
    fn the_paths_sit_under_marions_config_dir() {
        assert_eq!(home(Path::new("/cfg")), PathBuf::from("/cfg/home"));
        assert_eq!(
            mcp_config_path(Path::new("/cfg")),
            PathBuf::from("/cfg/mcp.json")
        );
    }

    /// A real run: `create` granted through `write`, then `marion-report` answered `recorded`.
    #[test]
    fn a_report_is_read_off_the_execution_start_frame_in_copilots_own_spelling() {
        let out = parse_stream(WRITE_THEN_REPORT, "marion-report");
        assert_eq!(
            out.narrative.as_deref(),
            Some("Wrote the matrix marker under src/ and reported back.")
        );
        assert_eq!(out.failure, None, "the call succeeded and the run exited 0");
        assert!(out.result_commits.is_empty());
        assert!(out.file_change_paths.is_empty(), "git is the authority");
        // The spelling is load-bearing: no other harness's name for the tool appears.
        assert!(!WRITE_THEN_REPORT.contains("mcp__marion__report"));
        assert!(!WRITE_THEN_REPORT.contains("marion_report"));
    }

    /// **S24's headline hazard**: a `report` the bridge answered `isError: true` — and a `report`
    /// denied for want of a grant — both end the run at `exitCode: 0`. The stream is the only place
    /// either failure is described.
    #[test]
    fn a_report_that_ended_in_error_is_a_failure_even_though_the_run_exited_zero() {
        for (label, stream, expect) in [
            (
                "isError",
                REPORT_ISERROR,
                "MCP server 'marion': refused: not authorized",
            ),
            (
                "denied",
                CREATE_DENIED,
                "Permission denied and could not request permission from user",
            ),
        ] {
            assert!(
                stream.contains(r#""exitCode":0"#),
                "{label}: the hazard's premise"
            );
            let out = parse_stream(stream, "marion-report");
            assert_eq!(
                out.narrative.as_deref(),
                Some("Wrote the matrix marker under src/ and reported back."),
                "{label}: the call was still made, and what it said is still the child's words"
            );
            let failure = out
                .failure
                .unwrap_or_else(|| panic!("{label}: must be a failure"));
            assert!(failure.contains(expect), "{label}: {failure}");
        }
    }

    /// The CLI's own failure claim, and the exit code it comes with.
    #[test]
    fn a_provider_failure_is_read_off_session_error() {
        let out = parse_stream(PROVIDER_500, "marion-report");
        assert_eq!(out.narrative, None, "no call was ever made");
        let failure = out.failure.expect("session.error is a failure");
        assert!(failure.contains("retried 5 times"), "{failure}");
        assert!(failure.contains("500"), "{failure}");
    }

    #[test]
    fn a_nonzero_result_exit_code_is_a_failure_when_nothing_more_specific_was_said() {
        let out = parse_stream(r#"{"type":"result","exitCode":1}"#, "marion-report");
        assert_eq!(
            out.failure.as_deref(),
            Some("copilot's result frame reported exitCode 1")
        );
        assert_eq!(
            parse_stream(r#"{"type":"result","exitCode":0}"#, "marion-report").failure,
            None
        );
    }

    /// The verb comes off `toolName` with the `marion-` prefix stripped, and the verdict off the
    /// completion with the same `toolCallId`. Native tools are not marion calls.
    #[test]
    fn marion_calls_are_paired_to_their_completions_by_tool_call_id() {
        assert_eq!(
            marion_calls(WRITE_THEN_REPORT, "marion-"),
            vec![MarionCall {
                verb: "report".into(),
                outcome: CallOutcome::Answered,
            }],
            "the `create` call is copilot's own and is not counted"
        );
        assert_eq!(
            marion_calls(REPORT_ISERROR, "marion-"),
            vec![MarionCall {
                verb: "report".into(),
                outcome: CallOutcome::Refused(
                    "MCP server 'marion': refused: not authorized".into()
                ),
            }]
        );
        assert_eq!(
            marion_calls(CREATE_DENIED, "marion-")[0].outcome,
            CallOutcome::Refused(
                "Permission denied and could not request permission from user".into()
            )
        );
    }

    #[test]
    fn a_call_whose_completion_never_arrived_is_unknown() {
        let stream = r#"{"type":"tool.execution_start","data":{"toolCallId":"c1","toolName":"marion-spawn","arguments":{}}}"#;
        assert_eq!(
            marion_calls(stream, "marion-"),
            vec![MarionCall {
                verb: "spawn".into(),
                outcome: CallOutcome::Unknown,
            }]
        );
        assert_eq!(marion_calls(PROVIDER_500, "marion-"), vec![]);
    }
}
