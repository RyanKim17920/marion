//! The Claude Code adapter (design §5.2): its launch row, its stream grammar, and its MCP
//! declaration document.
//!
//! Control is **config-time**: marion owns the launch configuration and never parses a pty. Every
//! flag here was measured against 2.1.220, and several are non-obvious enough that the tests below
//! state *why* rather than merely pinning the string.

use marion_core::agent_type;
use marion_core::harness::Harness;
use marion_core::provider::{KeyHeader, Wire};
use serde_json::{Value, json};

use crate::grammar::{
    ActivityRule, Cond, Failure, Name, OnRefusedReport, Pairing, SessionId, StreamGrammar,
    TextUnit, ToolUnit, UsageFold, UsageRule, Verdict, Where,
};
pub use crate::mcp_bridge::{
    AGENT_ID_ENV, AGENT_TYPE_ENV, AUTH_ENV, BASE_URL_ENV, BridgeEnv, DEPTH_ENV, NODE_TOKEN_ENV,
    READY_FILE_ENV,
};
use crate::spec::{
    Approval, Arg, BootDialog, BootDialogs, Constraint, Deliveries, DialogAnswer, Env, Field,
    HarnessSpec, LiveDeclaration, McpRoute, McpRoutes, MidTurn, Push, Resume, Spelling, Surfaces,
    ToolSpelling, TurnDelivery, UpdatePolicy, Val, When, WireRecipe,
};
use crate::surfaces::TypedKind;

/// The `--mcp-config` document's name under the node's own directory — one spelling for
/// [`SPEC`]'s live declaration and `ClaudeCodeAdapter::config_files`.
pub const MCP_CONFIG_FILE: &str = "mcp.json";

/// The `--settings` overlay every node marion launches carries: the operator's hooks off, their
/// settings otherwise as they wrote them. Not on the native facade, where the operator drives
/// their own claude.
pub const HOOKS_OFF_SETTINGS: &str = r#"{"disableAllHooks":true}"#;

/// [`mcp_config_json`] as the bytes `--mcp-config` reads: the live declaration's body.
pub fn mcp_config_document(b: &BridgeEnv) -> String {
    serde_json::to_string_pretty(&mcp_config_json(b)).expect("a Value always serialises")
}

/// Claude Code's row. Measured against 2.1.220 (S1, S9, S11) and re-measured on 2.1.222 for the
/// two tool axes (`tests/fixtures/s14/`); every flag below carries its reason in the doc of the
/// hand-written compile it was transcribed from, and those reasons are repeated here only where
/// the *shape* is the surprising part.
pub const SPEC: HarnessSpec = HarnessSpec {
    harness: Harness::ClaudeCode,
    surfaces: Surfaces::Headless(TypedKind::StreamJson),
    program: Some("claude"),
    argv: &[
        Arg::Lit("-p"),
        Arg::Lit("--output-format"),
        Arg::Lit("stream-json"),
        Arg::Lit("--input-format"),
        Arg::Lit("stream-json"),
        // MANDATORY with `-p --output-format stream-json`: without it 2.1.220 exits 1 with "When
        // using --print, --output-format=stream-json requires --verbose" before emitting anything.
        Arg::Lit("--verbose"),
        // The availability axis, **emitted even when empty**: `--tools ""` is the documented
        // "disable all tools" spelling, and every node ran with it before the axis existed. It does
        // not gate MCP tools.
        Arg::Joined("--tools", Field::Tools),
        // The permission axis. marion's tools must be listed here or a denied `spawn` is the result.
        Arg::Joined("--allowedTools", Field::Allowed),
        // Without this a non-allowlisted call is auto-denied in-process and no `can_use_tool`
        // frame ever reaches marion. Absent from --help.
        Arg::Lit("--permission-prompt-tool"),
        Arg::Lit("stdio"),
        // Only the MCP servers marion declared; never the user's.
        Arg::Lit("--strict-mcp-config"),
        Arg::Flag("--mcp-config", Field::McpConfig),
        // No user settings, plugins or hooks leak into a canned node. This does NOT suppress the
        // session-title request (§5.5). **Canned only**: a settings file is a credential source
        // (`apiKeyHelper`, `awsAuthRefresh`, an `env` block naming a key, Bedrock or a gateway),
        // and 2.1.280 measured the flag hiding it — a config dir whose `settings.json` alone held
        // the credential answered `Not logged in` with the flag and took the settings' route with
        // `--setting-sources=user`. A live node runs on the operator's settings as they wrote them.
        Arg::CannedLit("--setting-sources"),
        Arg::CannedLit(""),
        // None of the operator's hooks, user or plugin, run in a node marion launches: a blocking
        // `Stop` hook (a review gate) would hold or loop a node no one is watching. `--settings`
        // merges this one key over their layers, so the credential keys the line above keeps
        // (`apiKeyHelper`, the `env` block) still apply — measured on 2.1.283 (MILESTONES,
        // verified harness facts). Both modes: redundant under canned, where the exclusion above
        // already keeps hooks out. Hook callbacks marion registers in `initialize` still fire.
        Arg::Lit("--settings"),
        Arg::Lit(HOOKS_OFF_SETTINGS),
        Arg::Flag("--model", Field::Model),
        Arg::Resume,
    ],
    // The TUI: the same isolation and the same two axes, none of the protocol flags (a pane has an
    // operator in it, so the permission ask stays the harness's own dialog), and the prompt on
    // argv — seeded into the composer, not sent, so there is no turn-one race to lose.
    pane: Some(&[
        Arg::Joined("--tools", Field::Tools),
        Arg::Joined("--allowedTools", Field::Allowed),
        Arg::Lit("--strict-mcp-config"),
        Arg::Flag("--mcp-config", Field::McpConfig),
        Arg::CannedLit("--setting-sources"),
        Arg::CannedLit(""),
        Arg::Lit("--settings"),
        Arg::Lit(HOOKS_OFF_SETTINGS),
        Arg::Flag("--model", Field::Model),
        Arg::Resume,
        Arg::PosIfNonEmpty(Field::Prompt),
    ]),
    // NOT `CLAUDE_CONFIG_DIR`: isolating it breaks OAuth, because the Keychain entry is keyed to
    // the real config dir. The fileless path above is what keeps auth working, and live mode is
    // therefore *only* the removal of these three — which the neutral fields' live rule does.
    env: &[
        Env {
            key: "ANTHROPIC_BASE_URL",
            val: Val::Field(Field::BaseUrl),
            when: When::Always,
        },
        Env {
            key: "ANTHROPIC_AUTH_TOKEN",
            val: Val::Field(Field::ApiKey),
            when: When::Always,
        },
        // A non-empty key silently wins over the token (§6.4), so it is blanked beside one rather
        // than left inherited — otherwise a node would present the operator's real key to marion's
        // endpoint.
        Env {
            key: "ANTHROPIC_API_KEY",
            val: Val::Lit(""),
            when: When::Present(Field::ApiKey),
        },
        // Claude Code's background calls (titles, summaries) name a Haiku id by default. On a
        // third-party endpoint that is a Claude model sent to someone else, so both are the node's
        // own model there. Canned mode needs neither: marion's endpoint ignores the name.
        Env {
            key: "ANTHROPIC_SMALL_FAST_MODEL",
            val: Val::Field(Field::Model),
            when: When::Endpoint,
        },
        Env {
            key: "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            val: Val::Field(Field::Model),
            when: When::Endpoint,
        },
    ],
    stream: Some(&STREAM),
    // `read` → `Read`, `write` → `Write`, measured on 2.1.222 (`tests/fixtures/s14/`): `--tools
    // Write` puts a tool of that name with schema `{file_path, content}` into the request body, and
    // `--tools Read` puts `Read` there where the default `--tools ""` has none. **This is the one
    // harness where the grant buys something.** The negative half is why the mapping exists rather
    // than a pass-through: `--tools read`, marion's own word unmapped, yields `body.tools []`,
    // exit 0, empty stderr — indistinguishable from a healthy run. `Edit` and `Bash` were never
    // tried and are not named.
    tool_names: &[
        (agent_type::TOOL_READ, "Read"),
        (agent_type::TOOL_WRITE, "Write"),
    ],
    spelling: Spelling::Fixed(ToolSpelling::McpDoubleUnderscore),
    // `--mcp-config` names a document in both auth modes: live mode drops three env vars and
    // changes nothing about where the declaration lives.
    mcp: McpRoutes {
        canned: McpRoute::Document,
        live: McpRoute::Document,
    },
    // The same document the headless launch names, and the same flag, alone: `--strict-mcp-config`
    // is deliberately **not** beside it here, because on a node whose settings are the operator's
    // own it would drop every server they configured (§6.4).
    live_declaration: Some(LiveDeclaration::ArgvDocument {
        flag: "--mcp-config",
        file: MCP_CONFIG_FILE,
        prefix: "",
        body: mcp_config_document,
    }),
    // The one harness with a real per-tool allowlist: the record is the literal contents of
    // `--allowedTools`, which is the flag the CLI checks a call against.
    constraint: Constraint::Allowed { prefix: "" },
    // `-r, --resume [value]  Resume a conversation by session ID` (2.1.224 `--help`); the same flag
    // on the TUI.
    resume: Some(Resume::Flag("--resume")),
    // 2.1.263's own read, from the binary: `if (Ie(process.env.DISABLE_AUTOUPDATER)) return {type:
    // "env", envVar: "DISABLE_AUTOUPDATER"}` — a truthy parse (`1`/`true`/`yes`/`on`), and `"1"`
    // is the value claude sets for the children it launches itself. It gates the background
    // updater **and** the `Update available!` banner. `DISABLE_UPDATES` and
    // `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` disable it too and more besides; the settings
    // boolean `autoUpdates: false` survives only as a migration into this very variable.
    updates: UpdatePolicy::Env {
        key: "DISABLE_AUTOUPDATER",
        value: "1",
        note: "2.1.263 binary strings: the updater's disabled-reason check reads \
               `DISABLE_AUTOUPDATER` first among the switches; claude sets `\"1\"` for its own \
               children",
    },
    // 2.1.268 (probed 2026-09-11): a server's `notifications/claude/channel` becomes a new user
    // turn in the interactive session when claude is started with the development-channels flag
    // (one full-screen warning dialog at startup; default accepts). `-p` never enqueues it, so
    // the headless row above carries no flag. The name is what 2.1.268 sends in `initialize`'s
    // `clientInfo.name`.
    push: Push::ClaudeChannel,
    // `--allowedTools` is the list 2.1.220 checks a call against; marion's verbs are on it, and a
    // call to anything off it asks over `--permission-prompt-tool stdio` (S9).
    approval: Approval::AllowedToolsArg {
        flag: "--allowedTools",
        note: "S9 on 2.1.220: an allowlisted marion verb runs, an unlisted tool asks over \
               can_use_tool; s14 for --allowedTools as the permission axis",
    },
    client_name: Some("claude-code"),
    delivery: Deliveries {
        // S31 `p0a/b3`, `p0a/b`, `p0a/b2` (2.1.280, repeated on 2.1.276).
        headless: TurnDelivery::TypedTurn {
            mid_turn: MidTurn::Fold,
            note: "S31 p0a/b3,b,b2 (2.1.280/2.1.276): a stream-json user frame after `result` \
                   starts turn 2 in the same process and session; a frame written mid-turn is \
                   folded into the running turn at the next tool-result boundary with no \
                   `result` of its own, or queued as the next turn (its own `result`) when the \
                   in-flight request is the turn's last. Never dropped",
        },
        // S31 `p0a/ch2`. The fallback a later phase may take is measured (`p0b/tui/claude`).
        interactive: TurnDelivery::McpChannel {
            note: "S31 p0a/ch2 (2.1.280/2.1.276): notifications/claude/channel folds or queues \
                   like a typed frame. Channels need a claude.ai login: under API-key or token \
                   auth claude refuses them (\"Channels are not currently available\") and the \
                   message is not delivered; bracketed paste into the TUI is measured to work \
                   (p0b/tui/claude) and is the fallback a later phase may add",
        },
    },
    // S37 first screens (2.1.283, `tests/fixtures/s37-boot-dialogs/claude-code-2.1.283.raw`): a
    // fresh directory opens on folder trust, selection on `No, exit`. A bare CR quits the session
    // (measured: the process exited); down-arrow + CR in one write trusts and opens the composer.
    // The pane then shows the development-channels warning on a claude.ai login, which is held.
    boot_dialogs: BootDialogs {
        dialogs: &[
            BootDialog {
                needle: "❯ No, exit Yes, I trust this folder",
                answer: DialogAnswer::Keys(b"\x1b[B\r"),
                note: "S37 2.1.283 folder trust, default `No, exit`: `ESC[B` + CR as one write \
                       selects `Yes, I trust this folder` and the composer draws; CR alone exits",
            },
            BootDialog {
                needle: "Is this a project you created or one you trust?",
                answer: DialogAnswer::Hold,
                note: "the same dialog with a selection S37 did not measure",
            },
            BootDialog {
                needle: "WARNING: Loading development channels",
                answer: DialogAnswer::Hold,
                note: "S37 2.1.283 (`claude-code-2.1.283-channels.raw`): after folder trust, a \
                       claude.ai login shows this for the pane's `--dangerously-load-development-\
                       channels`, selection on `1. I am using this for local development`; an \
                       acknowledgement the operator gives, never marion. It appears wherever \
                       the operator's claude.ai login is present, a canned token beside it \
                       included (S37 P-tui); without that login channels are refused and it \
                       does not",
            },
        ],
        note: "S37 2.1.283, fresh directory, the operator's login and an isolated config with \
               onboarding done: folder trust, then (pane shape, claude.ai login) the development \
               channels warning",
    },
    wires: &[WireRecipe {
        wire: Wire::AnthropicMessages,
        env: &[],
        // `ANTHROPIC_AUTH_TOKEN` is sent as `Authorization: Bearer` and `ANTHROPIC_API_KEY` as
        // `X-Api-Key` (Claude Code's environment-variable docs); a non-empty key wins over a token,
        // so each recipe sets its own and blanks the other.
        keys: &[
            crate::spec::KeyRecipe {
                header: KeyHeader::Bearer,
                env: &[],
                note: "the row's `ANTHROPIC_AUTH_TOKEN`, beside a blanked `ANTHROPIC_API_KEY`",
            },
            crate::spec::KeyRecipe {
                header: KeyHeader::XApiKey,
                env: &[
                    Env {
                        key: "ANTHROPIC_API_KEY",
                        val: Val::Field(Field::ApiKey),
                        when: When::Always,
                    },
                    Env {
                        key: "ANTHROPIC_AUTH_TOKEN",
                        val: Val::Lit(""),
                        when: When::Always,
                    },
                ],
                note: "`ANTHROPIC_API_KEY` is sent as `X-Api-Key` (Claude Code docs, env vars); \
                       not run here: the installed claude is outside the version gate",
            },
        ],
        note: "Anthropic Messages alone: `ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN` is the one provider channel the row renders.",
    }],
    note: "S1/S9/S11 on 2.1.220; s14 on 2.1.222 for --tools/--allowedTools. The pane shape was \
           measured on 2.1.220 for M3 C1 (MILESTONES: the recorded manual session)",
};

/// How a `--output-format stream-json` stream is read (`tests/fixtures/s1/`, `s9/`).
///
/// A call to marion is a `tool_use` block inside an `assistant` frame's `message.content[]`, and
/// its result a `tool_result` block inside a later `user` frame, paired by `tool_use_id`. On the
/// recording the success case has **no `is_error` key at all**, which is why the verdict is an
/// error *flag*: an absent key is this harness saying the call was fine. The run's own verdict is
/// its `result` frame — `is_error` or a `subtype` other than `success` — read on this surface
/// rather than the exit code, because 2.1.220 reports its own errors in-band.
///
/// `file_changes` is `None`: Claude Code's edits arrive as `tool_use` blocks for its own built-in
/// tools, whose argument shapes are per-tool and unmeasured here, and git is the authority anyway.
pub const STREAM: StreamGrammar = StreamGrammar {
    call: Where {
        frame: &[Cond::Eq("/type", "assistant")],
        each: Some("/message/content"),
        unit: &[Cond::Eq("/type", "tool_use")],
    },
    name: Name::Prefixed("/name"),
    args: "/input",
    pairing: Pairing::Separate {
        call_id: "/id",
        result: Where {
            frame: &[Cond::Eq("/type", "user")],
            each: Some("/message/content"),
            unit: &[Cond::Eq("/type", "tool_result")],
        },
        result_id: "/tool_use_id",
        verdict: Verdict::ErrorFlag {
            path: "/is_error",
            words: &["/content"],
            fallback: "the result frame carried no message",
        },
    },
    refused_report: OnRefusedReport::Record,
    failures: &[
        Failure::NotOk {
            at: Where {
                frame: &[Cond::Eq("/type", "result")],
                each: None,
                unit: &[],
            },
            path: "/subtype",
            ok: "success",
            words: &["/result", "/subtype"],
            label: "the run's result frame reported an error",
        },
        Failure::Frame {
            at: Where {
                frame: &[Cond::Eq("/type", "result"), Cond::Eq("/is_error", "true")],
                each: None,
                unit: &[],
            },
            words: &["/result", "/subtype"],
            fallback: "the run's result frame reported an error",
        },
    ],
    file_changes: None,
    // The first frame of every run (`s10/stream-*.jsonl`): `system`/`init` carries `session_id`,
    // the value `--resume` takes back.
    session: Some(SessionId {
        at: Where {
            frame: &[Cond::Eq("/type", "system"), Cond::Eq("/subtype", "init")],
            each: None,
            unit: &[],
        },
        path: "/session_id",
        resumes_in_place: false,
    }),
    // Each turn's `result` frame (`s4/claude-code/stream-*.jsonl`, `s9`, `s10`) totals that turn:
    // S31 `p0a/out/a` measured two stream-json turns reporting 10/5 each in `usage` while
    // `modelUsage` ran cumulative, so a node that took several turns sums them. `input_tokens`
    // excludes both cache counters (Anthropic's convention). qwen shares this row and was measured
    // emitting the same shape without the cache-write key (`s25`); one result, so the sum is it.
    usage: Some(UsageRule {
        at: Where {
            frame: &[Cond::Eq("/type", "result")],
            each: None,
            unit: &[],
        },
        input: "/usage/input_tokens",
        output: "/usage/output_tokens",
        cache_read: Some("/usage/cache_read_input_tokens"),
        cache_write: Some("/usage/cache_creation_input_tokens"),
        input_includes_cache: false,
        fold: UsageFold::Sum,
    }),
    // `assistant` frames' content blocks (`s9/can-use-tool-*.stdout.jsonl`): a `tool_use` block is
    // a call to any tool, a `text` block the model's words. qwen shares this row.
    activity: Some(ActivityRule {
        calls: &[ToolUnit {
            at: Where {
                frame: &[Cond::Eq("/type", "assistant")],
                each: Some("/message/content"),
                unit: &[Cond::Eq("/type", "tool_use")],
            },
            name: "/name",
            args: "/input",
            id: Some("/id"),
        }],
        text: &[TextUnit {
            at: Where {
                frame: &[Cond::Eq("/type", "assistant")],
                each: Some("/message/content"),
                unit: &[Cond::Eq("/type", "text")],
            },
            path: "/text",
            joins: false,
        }],
    }),
};

/// `ANTHROPIC_BASE_URL` from the provider base URL marion carries.
///
/// The two harnesses disagree about the `/v1`: a Codex `model_providers` entry names the full
/// `…/v1`, while Claude Code appends `/v1/messages` to whatever it is given and would otherwise
/// request `/v1/v1/messages`. marion stores the Codex form — it is the one that appears verbatim
/// in a config file — and derives the other. This lives with the adapter that needs the
/// derivation, so the supervisor never has to know which harness wants which spelling.
pub fn anthropic_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    trimmed
        .strip_suffix("/v1")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_string()
}

/// The `--mcp-config` document declaring marion's control MCP.
///
/// The bridge is a *short-lived process the harness starts*, not one marion spawns (§5.4), so
/// everything it needs rides this declaration: which repo, which state dir, which provider, and
/// **which node it is serving**. `MARION_AGENT_ID` is what makes `TaskContract.requester` the
/// root's own `AgentId` rather than a placeholder.
///
/// This is the Claude Code adapter's config emission, so it lives here rather than in the
/// supervisor: §3.1 makes config generation part of the adapter contract, and the Codex adapter's
/// counterpart ([`crate::codex::config_toml`]) has always lived beside its own compile step. The
/// `env` block is [`BridgeEnv::env_json`], the same derivation every harness's document writes.
pub fn mcp_config_json(b: &BridgeEnv) -> Value {
    json!({
        "mcpServers": {
            "marion": {
                "type": "stdio",
                "command": b.bridge.to_string_lossy(),
                "args": b.args,
                "env": b.env_json()
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Auth;
    use crate::invocation::Invocation;
    use crate::spec::{Axes, Fields, Shape, render};

    /// A **root**, as the adapter's fields hook shapes it: no credential of its own (`marion run`
    /// mints a per-run token after `compile`, so a request log attributes traffic to one run), the
    /// `/v1`-less base URL the hook derives, and the declaration document under the node's config
    /// dir. The hook's own decisions — the argv-prompt refusal, the derivation — are pinned in
    /// `adapter::tests`; these tests pin **the row**.
    fn root() -> Fields {
        Fields {
            cwd: "/tmp/wt".into(),
            config_dir: "/tmp".into(),
            auth: Auth::Canned,
            prompt: String::new(),
            model: Some("haiku".into()),
            base_url: Some("http://127.0.0.1:8099".into()),
            api_key: None,
            axes: Axes {
                tools: vec![],
                allowed: vec!["mcp__marion__spawn".into(), "mcp__marion__status".into()],
                mode: None,
            },
            mcp_config: Some("/tmp/mcp.json".into()),
            ..Fields::default()
        }
    }

    /// A **child** differs from a root in exactly two compiled values — the permission axis it is
    /// given and the credential it presents — and in nothing else. Every flag is the root's,
    /// because a Claude Code node has one launch shape ([`SPEC`]).
    fn child() -> Fields {
        Fields {
            model: None,
            axes: Axes {
                allowed: vec!["mcp__marion__report".into()],
                ..Axes::default()
            },
            api_key: Some("dummy".into()),
            ..root()
        }
    }

    fn compile(f: &Fields) -> Invocation {
        render(&SPEC, Shape::Headless, f).unwrap()
    }

    #[test]
    fn verbose_is_present_because_the_cli_exits_1_without_it() {
        let inv = compile(&root());
        assert!(
            inv.args.iter().any(|a| a == "--verbose"),
            "2.1.220 refuses -p --output-format stream-json without --verbose"
        );
    }

    #[test]
    fn the_two_tool_axes_are_distinct() {
        let inv = compile(&root());
        // --tools is availability and does NOT gate MCP tools; --allowedTools is permission and
        // is what a denied mcp__marion__spawn call turns on. An empty availability axis is still
        // emitted, as the documented `""`.
        let i = inv.args.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(inv.args[i + 1], "");
        let i = inv.args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(inv.args[i + 1], "mcp__marion__spawn,mcp__marion__status");
    }

    #[test]
    fn permission_prompt_tool_is_set_or_can_use_tool_never_fires() {
        let inv = compile(&root());
        let i = inv
            .args
            .iter()
            .position(|a| a == "--permission-prompt-tool")
            .expect("without this the CLI auto-denies in-process and marion sees nothing");
        assert_eq!(inv.args[i + 1], "stdio");
    }

    #[test]
    fn mcp_config_is_strict_so_the_users_servers_stay_out() {
        let inv = compile(&root());
        assert!(inv.args.iter().any(|a| a == "--strict-mcp-config"));
        let i = inv.args.iter().position(|a| a == "--mcp-config").unwrap();
        assert_eq!(
            inv.args[i + 1],
            "/tmp/mcp.json",
            "under the node's own config dir"
        );
    }

    #[test]
    fn claude_config_dir_is_never_isolated() {
        let inv = compile(&root());
        assert!(
            !inv.env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"),
            "isolating it breaks OAuth: the Keychain entry is keyed to the real config dir"
        );
    }

    /// The hooks-off overlay is the one token with a `"` in it, and its quotes are JSON syntax
    /// that claude parses, not shell quoting around a value.
    #[test]
    fn nothing_is_shell_quoted_because_nothing_reaches_a_shell() {
        let inv = compile(&root());
        assert_eq!(inv.program, "claude");
        assert!(
            inv.args
                .iter()
                .filter(|a| *a != HOOKS_OFF_SETTINGS)
                .all(|a| !a.contains('\'') && !a.contains('"'))
        );
    }

    /// **The defect this shape exists to end.** A child's prompt is a frame written after the
    /// readiness gate, never argv: 2.1.220 does not hold turn one for an `--mcp-config` server, so
    /// an argv prompt takes that turn with `"tools":[]`, gets the session-title stub back, and the
    /// run exits 0 having called nothing (§6.1 step 8, §12).
    #[test]
    fn a_childs_prompt_is_never_compiled_into_argv_and_stdin_stays_typed() {
        let inv = compile(&child());
        assert!(
            inv.args.iter().any(|a| a == "--input-format"),
            "without a typed stdin there is no frame to withhold, and the gate cannot exist"
        );
        let i = inv.args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(
            inv.args.get(i + 1).map(String::as_str),
            Some("--output-format"),
            "`-p` takes no value here; a positional prompt is the toolless-turn bug"
        );
        // And a prompt that does arrive is refused before this row is rendered:
        // `adapter::tests::a_claude_child_is_refused_if_its_prompt_was_compiled_into_argv`.
    }

    /// The permission axis is the one that decides whether the child's single load-bearing call
    /// happens at all, and its failure is silent: an unlisted tool is auto-denied in process.
    #[test]
    fn a_childs_marion_tools_are_allowlisted() {
        let inv = compile(&child());
        let i = inv.args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(inv.args[i + 1], "mcp__marion__report");
    }

    #[test]
    fn a_childs_credential_is_the_token_and_the_key_beside_it_is_blanked() {
        let inv = compile(&child());
        assert_eq!(
            inv.env,
            vec![
                (
                    "ANTHROPIC_BASE_URL".to_string(),
                    "http://127.0.0.1:8099".to_string()
                ),
                ("ANTHROPIC_AUTH_TOKEN".to_string(), "dummy".to_string()),
                // Non-empty, it would silently win — and would be the operator's own key.
                ("ANTHROPIC_API_KEY".to_string(), String::new()),
                // The row's update policy: no background updater under a running node.
                ("DISABLE_AUTOUPDATER".to_string(), "1".to_string()),
            ]
        );
    }

    /// And a caller that mints its own per-run token after `compile` — which is what `marion run`
    /// does, so that a request log attributes traffic to one run — gets no pair pushed under it.
    #[test]
    fn a_node_with_no_credential_in_its_spec_carries_no_anthropic_pair() {
        let inv = compile(&root());
        let names: Vec<&str> = inv.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["ANTHROPIC_BASE_URL", "DISABLE_AUTOUPDATER"]);
    }

    #[test]
    fn a_node_records_the_model_it_was_given_and_nothing_when_it_was_given_none() {
        assert_eq!(compile(&child()).model, None);
        let inv = compile(&Fields {
            model: Some("haiku".into()),
            ..child()
        });
        assert_eq!(inv.model.as_deref(), Some("haiku"));
        let i = inv.args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(inv.args[i + 1], "haiku");
    }
}
