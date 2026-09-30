//! The Codex adapter — two surfaces, `exec` and the TUI (design §9).
//!
//! `codex exec` is **launch-only with protocol events**: marion writes its configuration, starts
//! it, and reads its JSONL stream. There is no control channel to steer it mid-turn. That is M1's
//! child ([`SPEC`]'s `argv`) and it is the shape every codex node runs under unless a run asked
//! otherwise.
//!
//! The interactive `codex` is **opaque**: a pty and nothing else, driven by keystrokes and parsed
//! from nothing ([`SPEC`]'s `pane`). It exists for §9's M3 criterion C2, and it is a *per-run*
//! request rather than a second harness — the two share this file's configuration, its isolation
//! and its sandbox, and differ only in the argv grammar the binary's two commands accept.

use std::path::{Path, PathBuf};

use marion_core::agent_type;
use marion_core::harness::Harness;
use marion_core::provider::Wire;

use crate::adapter::{
    HarnessAdapter, HarnessError, LaunchSpec, McpDeclaration, Row, SpawnCtx, declared_bridge,
};
use crate::auth::Auth;
use crate::caps::Capabilities;
use crate::env_filter::{EnvGrant, LoginEnv};
use crate::grammar::{
    ActivityRule, CallEnd, CallShape, Cond, ErrorRule, Name, OnRefusedReport, Pairing, PathList,
    Reasoning, SessionId, StreamGrammar, TextUnit, ToolUnit, UsageFold, UsageRule, Verdict, Where,
};
pub use crate::mcp_bridge::BridgeEnv;
use crate::profile::{ProfileCarrier, Status as ProfileStatus};
use crate::rpc_channel::{Answer, ReadyGate, RpcChannel, Turns};
use crate::spec;
use crate::spec::{
    Advertised, Approval, Arg, AxesRule, BootDialog, BootDialogs, BootSignal, Constraint,
    Deliveries, DialogAnswer, Env, Field, HarnessSpec, LiveDeclaration, McpRoute, McpRoutes,
    MidTurn, ModelForm, Push, ReadOnly, Readiness, Remembers, Resume, Spelling, Surfaces,
    TokenCarrier, TokenCarriers, ToolSpelling, TurnDelivery, UpdatePolicy, Val, When, WireRecipe,
};

/// Codex's row: the `exec` shape (S6, 0.146.0) and the TUI (M3 C2, 0.147.0), two argv grammars of
/// one binary over one isolation.
///
/// # The TUI is not `exec` with a flag off
///
/// `exec`'s `--json`, `--skip-git-repo-check`, `--output-schema` and `--output-last-message` are the
/// whole of what makes a headless codex a protocol peer, and **none of the four exists on the
/// interactive command** — `codex --help` on 0.147.0 lists them nowhere, so passing any of them is
/// an argv the binary rejects before it draws anything. The two `exec`-only outputs are therefore
/// absent from the pane row rather than compiled: a caller that sets one on a pane launch finds it
/// ignored, structurally.
///
/// # The prompt rides argv on both, and is **submitted** rather than seeded on the TUI
///
/// Measured on 0.147.0 against an isolated `CODEX_HOME`: `codex "<prompt>"` opens the TUI with the
/// text already sent — the composer shows it above a running spinner, with no keystroke from
/// anybody. That is the one place the two panes genuinely differ, because Claude Code's TUI seeds
/// its composer and waits (`crate::claude_code::SPEC`'s pane row). An operator who runs `marion run
/// codex --pane --prompt …` has taken a turn by the time they attach. An empty prompt compiles no
/// positional at all, which is a TUI opened at its composer.
///
/// # What is deliberately **not** on the pane row
///
/// **`--no-alt-screen`.** 0.147.0 documents it as *"Disable alternate screen mode … preserving
/// terminal scrollback history"*, which reads like exactly what C2 wants — and passing it would
/// make marion's scrollback claim a property of a flag marion chose rather than of the harness
/// marion has to survive. Measured on 0.147.0, the default is already inline: a full boot, a
/// submitted turn, two slash commands and a resize emit **zero** `ESC[?1049h`, which is §5.3's
/// reading of 0.146.0 unchanged. §5.3 also warns against treating "no alt screen" as a static
/// per-harness property — codex enters one transiently for `/diff` — so the emulator must handle
/// the switch either way, and a flag here would only hide whether it does.
///
/// **`-s/--sandbox` and `-a/--ask-for-approval`.** Neither flag is used. A canned node sets both
/// in the generated [`config_toml`] (`sandbox_mode`, `approval_policy`); a live node carries the
/// sandbox as the `-c sandbox_mode=…` pair ([`live_sandbox_override`]), the same key the document
/// writes, and keeps the operator's approval policy. A second spelling on argv is the drift
/// [`SANDBOX_MODE`] exists to prevent.
pub const SPEC: HarnessSpec = HarnessSpec {
    harness: Harness::Codex,
    // Typed turns over `codex app-server` ([`APP`]); `exec` is kept as [`EXEC`].
    surfaces: Surfaces::AppServer(&APP),
    program: Some("codex"),
    argv: &[
        Arg::Lit("app-server"),
        // The live route's whole configuration and the no-self-update switch, one `-c key=value`
        // per pair, exactly as `exec` takes them: `-c` is global on 0.155.1 (`codex app-server
        // --help`), and app-server reads `mcp_servers.marion.*` from argv like `config.toml` (S36
        // P3). Empty but for the update pair under canned, whose settings live in the document.
        Arg::Each("-c", Field::Pairs),
        // app-server has no `-m`; the model is the same configuration key `-m` sets.
        Arg::Pair("-c", "model", Field::Model),
    ],
    pane: Some(&[
        Arg::Each("-c", Field::Pairs),
        Arg::Flag("-m", Field::Model),
        // `-C/--cd` rather than relying on `Invocation.cwd` alone: codex names this *"the directory
        // the agent uses as its working root"*, which is what its sandbox is scoped to, and leaving
        // it to the process cwd would make the workspace a fact about who launched marion.
        Arg::Flag("-C", Field::Cwd),
        Arg::PosIfNonEmpty(Field::Prompt),
    ]),
    // `$CODEX_HOME`: the node's own agent dir where marion owns the config surface, and under
    // [`Auth::Inherited`] **not set at all** — not set to the operator's home, *unset*, so codex
    // resolves its own default. That is the whole of live auth on this harness: `CODEX_HOME` is
    // where codex looks for `auth.json`, and S8 measured codex's to be a plain 0600 file rather
    // than a Keychain item, so leaving the variable alone is enough for the child to find the login
    // the operator already has. Omitted, never blanked: an empty `CODEX_HOME` would send codex
    // looking for `auth.json` in the process's cwd.
    env: &[
        Env {
            key: "CODEX_HOME",
            val: Val::Under(""),
            when: When::Overlay,
        },
        // The variable the generated provider's `env_key` names ([`config_toml`]), carrying the
        // launch's own credential: the canned placeholder or per-run token, or an endpoint node's
        // stored key. Omitted, never blank, where the launch carries none.
        Env {
            key: PROVIDER_KEY_ENV,
            val: Val::Field(Field::ApiKey),
            when: When::Overlay,
        },
    ],
    stream: Some(&APP_STREAM),
    // `write` → `sandbox:workspace-write`, §3.1's *"coarsest equivalent"* named for this exact
    // harness: `codex exec` has no `--tools` and no permission list, only `--sandbox`, and marion
    // compiles `workspace-write` on every node — into [`config_toml`] when canned, as the
    // [`live_sandbox_override`] `-c` pair when live — so a declaration is **satisfied rather than
    // newly granted** and adds nothing further. Making the mode conditional would silently demote
    // every codex node marion spawns today to `read-only`.
    //
    // `read` is refused, and the absence is the decision: s14 measured codex's whole declaration
    // (`apply_patch, create_goal, exec_command, …`) identical under both sandbox modes with no
    // read tool in it — reading a file is `exec_command`, the shell, which also writes and reaches
    // the network. A reader of `tools: [read]` would take a codex node for read-only when it is
    // nothing of the kind.
    //
    // `edit` and `bash` are satisfied the same way: `apply_patch` and `exec_command` are in the
    // declaration under every sandbox mode (s14), and the sandbox is what bounds them.
    tool_names: &[
        (agent_type::TOOL_WRITE, "sandbox:workspace-write"),
        (agent_type::TOOL_EDIT, "sandbox:workspace-write"),
        (agent_type::TOOL_BASH, "sandbox:workspace-write"),
    ],
    // Flat, as on Claude Code: S6 measured codex running **code mode**, where a model reaches marion
    // by writing `await tools.mcp__marion__report({…})`. The `{"name","namespace"}` pair is codex's
    // internal wire dispatch form, never something a child types.
    spelling: Spelling::Fixed(ToolSpelling::McpDoubleUnderscore),
    // A canned node's `[mcp_servers.marion]` lives in the generated `config.toml`; a live node's
    // rides `-c mcp_servers.marion.…` on its own command line, because with `CODEX_HOME` unset the
    // only `config.toml` codex reads is the operator's own (§6.4).
    mcp: McpRoutes {
        canned: McpRoute::Document,
        live: McpRoute::Argv(MCP_SERVER_KEY),
    },
    live_declaration: Some(LiveDeclaration::ArgvPairs {
        flag: "-c",
        key: MCP_SERVER_KEY,
        pairs: live_config_overrides,
    }),
    // A canned node's token sits in its 0600 `config.toml`. A live node's declaration is argv,
    // so its token rides codex's environment and the `-c` pairs name it in `env_vars`
    // ([`live_config_overrides`]).
    // Its OpenAI key; a custom `model_providers.*.env_key` is the operator's `env_passthrough`.
    login_env: LoginEnv {
        login: &[
            EnvGrant::always("OPENAI_API_KEY"),
            EnvGrant::always("CODEX_API_KEY"),
            EnvGrant::always("OPENAI_BASE_URL"),
        ],
        any_provider: false,
    },
    token: TokenCarriers {
        canned: TokenCarrier::Declaration,
        live: TokenCarrier::ForwardedEnv {
            note: "codex 0.145.0 through 0.155.1 (2026-09-27, stub MCP server): a stdio server \
                   gets only HOME, LANG, LOGNAME, PATH, SHELL, TERM, TMPDIR, USER of the parent's \
                   environment, plus the variables `env_vars` names; `-c \
                   mcp_servers.<name>.env_vars=[…]` is parsed on argv too",
        },
    },
    // §3.1's worked example, verbatim: it replaces a hardcoded `["apply_patch", "shell"]` that
    // named tools codex never checked a call against. Constant, because marion compiles that one
    // sandbox mode on every node ([`config_toml`] canned, [`live_sandbox_override`] live).
    constraint: Constraint::Fixed {
        prefix: "sandbox:",
        value: SANDBOX_MODE,
    },
    // The TUI's `codex resume` is a different grammar and unmeasured, so the pane row carries no
    // `Arg::Resume` and a paned resume is refused.
    // A resume is app-server's own `thread/resume` ([`APP`]), never argv (S36 P8: after a SIGKILL
    // and relaunch it carries the thread's history, and it reopens `exec` threads too).
    resume: None,
    // codex 0.147.0 has **no** update-related variable in its `CODEX_*` list; the switch is the
    // config key `check_for_update_on_startup` (a boolean in its `ConfigToml` field list, and
    // runtime-typed: `-c check_for_update_on_startup=notabool` fails with "expected a boolean",
    // where an unknown key is ignored silently). `-c` is a global flag, so the pair rides the
    // row's own override channel on `exec`, on `exec resume` and on the TUI, and in the native
    // prefix. What it silences is the TUI-only startup check that shows `Update available!` and
    // installs on Enter — the prompt a scripted Enter hit mid-experiment (0.145.0 → 0.146.0).
    updates: UpdatePolicy::Pair {
        key: "check_for_update_on_startup",
        value: "false",
        note: "0.147.0 binary strings (`ConfigToml` field list) and a runtime type check on the \
               key; no `CODEX_*` update variable exists",
    },
    // Unmeasured: MCP's own logging notification, which this harness may show or drop.
    push: Push::McpLog,
    // On marion's own `[mcp_servers.marion]` block, in the document and on the live `-c` pairs
    // alike: without it every marion call is cancelled silently (S6).
    approval: Approval::DeclarationKey {
        key: "default_tools_approval_mode",
        contest: None,
        note: "S6 on 0.146.0: without `default_tools_approval_mode = \"approve\"` every marion \
               tool call is cancelled and the run ends Unreported",
    },
    // s38 (0.155.1): `sandbox_mode = "read-only"` refuses `apply_patch` and the shell's writes alike,
    // where `write` → `workspace-write` (the whole availability axis) would be satisfied anyway.
    // On `-c`, after the launch's own pairs, so it wins over [`live_sandbox_override`] and over
    // the generated config's `workspace-write`.
    read_only: ReadOnly::Pair {
        key: SANDBOX_KEY,
        value: READ_ONLY_SANDBOX,
        note: "s38 on 0.155.1: `sandbox_mode = \"read-only\"` rejects a scripted `apply_patch` \
               (`patch rejected: writing is blocked by read-only sandbox`) and seatbelt refuses \
               `exec_command`'s `printf > file` (`operation not permitted`); no file either way",
    },
    client_name: None,
    // Printed by every `exec` (codex-cli 0.155.1, live smoke 2026-09-27), clean runs included.
    stderr_boilerplate: &["Reading additional input from stdin..."],
    delivery: Deliveries {
        headless: TurnDelivery::TypedTurn {
            mid_turn: MidTurn::Fold,
            note: "S36 P6 (app-server 0.155.1): `turn/steer` answers at once mid tool, while a \
                   request is held and during an approval, and the text is a user message in the \
                   next request of the same turn, with one `turn/completed`; `turn/start` on an \
                   active turn steers too",
        },
        // 0.155.1 draws a provisional composer (`tui/src/startup_draft.rs`) the moment it starts,
        // then goes quiet while its app server boots: 1.25 s idle unloaded, longer under load. It
        // takes a paste and drops Enter and Tab, and carries the text into the real composer
        // unsubmitted. The real chat widget's constructor sets the window title; the draft never
        // does, so the title marks boot.
        interactive: TurnDelivery::bracketed_paste(
            BootSignal::WindowTitle,
            "S31 p0b/tui/codex (0.147.0): bracketed paste + CR submits at 0 ms; unbracketed text \
             + CR does not (the paste-burst heuristic eats the CR); busy repaints ≤ 114 ms. \
             0.155.1: a paste + CR on the startup draft (first draw, before the window title) is \
             never submitted; after the title and 1.5 s quiet it is",
        ),
    },
    // S37 first screens (0.155.1, `tests/fixtures/s37-boot-dialogs/codex-0.155.1.raw`): a fresh
    // `CODEX_HOME` and directory open on directory trust, selection on `1. Yes, continue`. The
    // S37 pane probe saw it swallow the first paste; CR trusts (config.toml gains the project's
    // `trust_level = "trusted"`) and the composer draws. In a git worktree the trust is keyed on
    // the **main repository** (0.155.1, 2026-09-27: CR in `repo-wt` wrote `[projects."<repo>"]`,
    // no worktree table; with the repo trusted the worktree showed no dialog and config.toml was
    // unchanged). marion answers it only where `CODEX_HOME` is its own (canned, endpoint).
    boot_dialogs: BootDialogs {
        dialogs: &[
            BootDialog {
                needle: "› 1. Yes, continue 2. No, quit",
                action: "trust {repo} in codex once (run `codex` there and choose `Yes, continue`); worktrees inherit it",
                answer: DialogAnswer::Keys(b"\r"),
                note: "S37 0.155.1 directory trust, default `1. Yes, continue`: CR trusts",
            },
            BootDialog {
                needle: "Do you trust the contents of this directory?",
                action: "trust {repo} in codex once (run `codex` there and choose `Yes, continue`); worktrees inherit it",
                answer: DialogAnswer::Hold,
                note: "the same dialog with a selection S37 did not measure",
            },
        ],
        remembers: Remembers::CannedHome,
        note: "S37 0.155.1, fresh CODEX_HOME and directory: directory trust is the only dialog \
               before the composer",
    },
    wires: &[WireRecipe {
        wire: Wire::OpenAiResponses,
        env: &[],
        keys: &[crate::spec::BEARER_BY_OVERLAY],
        note: "OpenAI Responses alone: `wire_api = \"chat\"` was removed from codex, and the generated provider names `responses`.",
    }],
    // Measured on 0.155.1 (2026-09-27): a fresh `CODEX_HOME` answers `login status` with
    // `Not logged in` (exit 1). `auth.json` lives in that directory and is never shared.
    profile: Some(ProfileCarrier {
        env: "CODEX_HOME",
        clear: &[],
        status: ProfileStatus::TextAbsent {
            argv: &["login", "status"],
            text: "Not logged in",
        },
        login_hint: "login",
        home_default: ".codex",
        shared: &["config.toml", "AGENTS.md"],
        note: "codex 0.155.1 `login status` on a fresh CODEX_HOME: Not logged in",
    }),
    note: "S36 on codex 0.155.1 for app-server over stdio (tests/fixtures/app-server-0.155.1); \
           S6 on 0.146.0 for exec --json (tests/fixtures/s6), kept as EXEC; the TUI row and its \
           omissions measured on 0.147.0 for M3 C2; tests/codex_app_server.rs, harness_matrix's \
           codex cell and M1's hop run the app-server row end to end",
    requires: &[],
    axes: AxesRule::Split,
    // `-m` is compiled only where a real endpoint serves the model it is asked for: canned compiles
    // none, so every canned contract records `None` and the canned argv is byte-identical to what
    // it was before `-m` was known to exist here.
    model: ModelForm::OmitUnderCanned,
    readiness: Readiness::Ungated,
    // §9: "`codex exec resume [SESSION_ID] [PROMPT]` exists on 0.146.0 and is exactly
    // `continue_()` + `prompt()`". That is a claim about the *binary*; the surface it is asked
    // for on is what decides whether it is publishable, and on `codex exec --json` it is not.
    //
    // S36 on 0.155.1, over `codex app-server`: `turn/steer` folds a message into the running
    // turn (P6), `turn/interrupt` ends it at once (P7), and `thread/tokenUsage/updated` reports
    // the thread's running total after every response (P4) — marion has driven all three
    // (`tests/codex_app_server.rs`). Nothing earlier than that version is claimed. [`EXEC`]
    // inherits the claim and its `LaunchOnly` ceiling clips it.
    advertised: Advertised {
        always: Capabilities::NONE,
        from_version: &[
            (
                Capabilities {
                    resume: true,
                    ..Capabilities::NONE
                },
                (0, 146, 0),
            ),
            (
                Capabilities {
                    steer: true,
                    interrupt: true,
                    usage: true,
                    ..Capabilities::NONE
                },
                (0, 155, 1),
            ),
        ],
    },
    // `sandbox_mode = "workspace-write"` on every node marion configures, and `codex exec` has no
    // per-tool knob at all (`SANDBOX_MODE`).
    writes_without_grant: true,
    // Measured 2026-09-29: `codex sandbox -c sandbox_mode="workspace-write" -- <cmd>` (0.155.1) ran a write in its cwd
    // and refused one under $HOME ("Operation not permitted"); the same seatbelt that bounds a node.
    containment: crate::containment::ContainmentRule::HarnessSandbox {
        verify: &["sandbox", "-c", "sandbox_mode=\"workspace-write\"", "--"],
    },
    // codex's own `-s/--sandbox read-only`: its sandbox refuses every write.
    read_only_modes: &[crate::authority::ReadOnlyMode {
        flags: &["-s", "--sandbox"],
        values: &["read-only"],
    }],
};

/// **The `codex exec` row**: the prompt rides argv, the JSONL stream is read, and a later turn is
/// an `exec resume <thread>` relaunch (S6, S31). [`SPEC`] with its channel removed, kept so that
/// falling back is one edit of row data, and so the exec fixtures keep a row to be read against.
pub const EXEC: HarnessSpec = HarnessSpec {
    // `LaunchOnly` + `ProtocolEvents` + no display — §3.4's combination outside the four presets.
    surfaces: Surfaces::LaunchOnly,
    argv: EXEC_ARGV,
    stream: Some(&STREAM),
    resume: Some(Resume::Subcommand("resume")),
    delivery: Deliveries {
        headless: TurnDelivery::Continuation {
            note: "S31 p0b/codex (0.147.0): `exec -C <cwd> resume <id>` continues the thread; the \
                   -c mcp_servers.marion.* redeclaration must ride every resume, and stdin is read \
                   once before the first request, never mid-run",
        },
        ..SPEC.delivery
    },
    note: "S6 on codex 0.146.0 for exec --json (tests/fixtures/s6), the headless row before S36",
    ..SPEC
};

/// `codex exec`'s argv ([`EXEC`]).
const EXEC_ARGV: &[Arg] = &[
    Arg::Lit("exec"),
    // **Ahead of the subcommand, because `exec resume` does not take it** (0.147.0,
    // `tests/fixtures/s29/`): `-C/--cd` is on `codex exec --help` and absent from `codex exec
    // resume --help`, and `exec resume <id> … -C <dir>` exits 2 with "unexpected argument
    // '-C'". `codex exec [OPTIONS] <COMMAND> [ARGS]` accepts it before `resume`, and a fresh
    // `exec` reads its flags in any order, so one position serves both launches.
    Arg::Flag("-C", Field::Cwd),
    // `codex exec resume [SESSION_ID] [PROMPT]` (0.147.0): a subcommand of `exec`, ahead of
    // the flags below, every one of which the resume help lists too (`--json`,
    // `--skip-git-repo-check`, `-c`, `-m`, `--output-schema`, `-o`) and the s29 probe accepted
    // after `resume <id>`.
    Arg::Resume,
    Arg::Lit("--json"),
    Arg::Lit("--skip-git-repo-check"),
    // The live route's whole configuration, one `-c key=value` per pair
    // ([`live_config_overrides`]); empty under canned, where the same settings are written into
    // the generated `config.toml` instead.
    Arg::Each("-c", Field::Pairs),
    // **Verified on 0.146.0**, where `codex exec --help` lists `-m, --model <MODEL>`. The
    // adapter places none under a canned provider, so every contract this harness has ever
    // written still records `None`; under a real vendor the model is the operator's to choose.
    Arg::Flag("-m", Field::Model),
    // `--output-schema`, the §9 fallback branch. S6 proved the primary branch, so M1 leaves it
    // unset.
    Arg::Flag("--output-schema", Field::OutputSchema),
    Arg::Flag("--output-last-message", Field::OutputLastMessage),
    Arg::Pos(Field::Prompt),
];

/// How a `codex exec --json` stream is read (`tests/fixtures/s6/`).
///
/// Codex is the one harness that does **not** name marion's verbs by a prefixed identifier: an
/// `mcp_tool_call` item carries `server` and `tool` as two fields, so the row selects marion's
/// server and reads the bare verb — a substring scan for `mcp__marion__` would find nothing here.
///
/// **One call is two frames of the same item, and the later one revises the earlier.**
/// `exec-mcp-report.stream.jsonl` records `item.started` with `"status":"in_progress"` and then
/// `item.completed` with `"status":"completed"`, so the item's `id` keys the call and the last
/// frame for it wins — the reader this replaces once counted both. `status` is the verdict and
/// `error` the words; only the success spelling is recorded, so anything else terminal is a
/// refusal rather than an unknown.
///
/// **One failure claim**: a `turn.failed` frame, in its `error.message` (measured on 0.155.1
/// against a provider refusing the key). Otherwise the contract's status comes from the reported
/// narrative and the exit code, as it always has.
/// `file_change` items are recorded as corroboration; git is the authority.
pub const STREAM: StreamGrammar = StreamGrammar {
    call: Where {
        frame: &[
            Cond::Eq("/item/type", "mcp_tool_call"),
            Cond::Eq("/item/server", MCP_ALIAS),
        ],
        each: None,
        unit: &[],
    },
    name: Name::Verb("/item/tool"),
    args: "/item/arguments",
    pairing: Pairing::SameUnit {
        id: Some("/item/id"),
        verdict: Verdict::Status {
            path: "/item/status",
            ok: "completed",
            pending: &["in_progress"],
            words: &["/item/error"],
        },
    },
    refused_report: OnRefusedReport::Record,
    failures: &[crate::grammar::Failure::Frame {
        at: Where {
            frame: &[Cond::Eq("/type", "turn.failed")],
            each: None,
            unit: &[],
        },
        words: &["/error/message"],
        fallback: "the child's stream carried a turn.failed frame",
    }],
    // S37 (`codex-0.155.1/p-errors-*.jsonl`): every provider fault is a `type: error` frame in
    // codex's words (`Reconnecting... 1/5 (unexpected status 401 …)`, `exceeded retry limit,
    // last status: 429 …`, a 500 as `We’re currently experiencing high demand …`), then
    // `turn.failed`.
    errors: &[
        ErrorRule {
            at: Where {
                frame: &[Cond::Eq("/type", "error")],
                each: None,
                unit: &[],
            },
            status: None,
            // `usage_limit_exceeded` / `usage_limit_reached` on an account's spent window.
            kind: Some("/codex_error_info"),
            words: &["/message"],
        },
        ErrorRule {
            at: Where {
                frame: &[Cond::Eq("/type", "turn.failed")],
                each: None,
                unit: &[],
            },
            status: None,
            kind: None,
            words: &["/error/message"],
        },
    ],
    file_changes: Some(PathList {
        at: Where {
            frame: &[Cond::Eq("/item/type", "file_change")],
            each: None,
            unit: &[],
        },
        list: "/item/changes",
        path: "/path",
    }),
    // The first JSON frame of `codex exec --json` (`s7/exec-spawn-child.stream.jsonl`):
    // `thread.started` carries `thread_id`, the value `exec resume` takes back.
    // No frame of `exec --json` names the model (`s4`, `s6`): the model is the launch's or the
    // operator's configured default, and marion records what argv carried.
    model: None,
    session: Some(SessionId {
        at: Where {
            frame: &[Cond::Eq("/type", "thread.started")],
            each: None,
            unit: &[],
        },
        path: "/thread_id",
        resumes_in_place: false,
        by_title: None,
    }),
    // One `turn.completed` per turn (`s4/codex/stream-*.jsonl`), carrying the **thread's** running
    // total rather than the turn's: a resumed `exec resume` turn reported both generations' spend
    // (0.155.1, `continuation.rs`: two canned responses per generation, and the second
    // generation's turn counted four). `input_tokens` **counts** `cached_input_tokens` (14997 of
    // which 11008 cached in `stream-none.jsonl`), and the reader takes the cache back out.
    // `reasoning_output_tokens` is part of `output_tokens`, as OpenAI counts completion tokens.
    usage: Some(UsageRule {
        at: Where {
            frame: &[Cond::Eq("/type", "turn.completed")],
            each: None,
            unit: &[],
        },
        input: "/usage/input_tokens",
        output: "/usage/output_tokens",
        cache_read: Some("/usage/cached_input_tokens"),
        cache_write: Some("/usage/cache_write_input_tokens"),
        reasoning: Some(Reasoning::Within("/usage/reasoning_output_tokens")),
        input_includes_cache: true,
        fold: UsageFold::Session,
        in_flight: None,
    }),
    // Every item kind the captures show work as (`s6/exec-*.stream.jsonl`, `s7`): an MCP call on
    // any server, a shell command, a patch. Each appears as `item.started` then `item.completed`
    // under one `id`, so the id keeps it one call, and the completed item's `status` (and a
    // command's `exit_code`) is its end. The model's words are the `agent_message` item.
    // No frame measured carrying the account's usage window.
    rate_limit: None,
    activity: Some(ActivityRule {
        calls: &[
            ToolUnit {
                at: Where {
                    frame: &[Cond::Eq("/item/type", "mcp_tool_call")],
                    each: None,
                    unit: &[],
                },
                name: "/item/tool",
                args: "/item/arguments",
                id: Some("/item/id"),
                shape: CallShape::Tool,
                end: Some(ITEM_END),
            },
            ToolUnit {
                at: Where {
                    frame: &[Cond::Eq("/item/type", "command_execution")],
                    each: None,
                    unit: &[],
                },
                name: "/item/type",
                args: "/item/command",
                id: Some("/item/id"),
                shape: CallShape::Command,
                end: Some(COMMAND_END),
            },
            ToolUnit {
                at: Where {
                    frame: &[Cond::Eq("/item/type", "file_change")],
                    each: None,
                    unit: &[],
                },
                name: "/item/type",
                args: "/item/changes",
                id: Some("/item/id"),
                shape: CallShape::Files,
                end: Some(ITEM_END),
            },
        ],
        text: &[TextUnit {
            at: Where {
                frame: &[Cond::Eq("/item/type", "agent_message")],
                each: None,
                unit: &[],
            },
            path: "/item/text",
            joins: false,
        }],
    }),
};

/// How a codex item ends: its `status` once `item.completed` carries it (`s6`, and the dry run's
/// `command_execution` items, which finish `completed` or `failed`; `declined` is a command the
/// sandbox refused).
const ITEM_END: CallEnd = CallEnd {
    status: "/item/status",
    ok: &["completed"],
    failed: &["failed", "declined"],
    exit: None,
};

/// [`ITEM_END`] for a `command_execution` item, which also states its `exit_code`.
const COMMAND_END: CallEnd = CallEnd {
    exit: Some("/item/exit_code"),
    ..ITEM_END
};

/// **`codex app-server`'s vocabulary** (S36, `tests/fixtures/app-server-0.155.1/`, 0.155.1).
///
/// - `initialize` then the `initialized` notification (P2); frames carry no `jsonrpc` member.
/// - A thread is `thread/start {cwd, ephemeral: false, sandbox}`, persisted so a later
///   `thread/resume {threadId}` reopens it with its history after a relaunch (P8: an ephemeral
///   thread is `no rollout found`). **`sandbox` rides the request** because it beats both
///   `config.toml` and argv `-c sandbox_mode` (P3), so the mode the contract records is the one the
///   thread runs under whatever the operator's configuration says.
/// - **The first turn waits for `mcpServer/startupStatus/updated` `ready` for marion's server**
///   (P3): the turn's first request otherwise goes out ~1 s after start without marion's tools.
/// - `turn/start` opens a turn; `turn/steer {expectedTurnId}` folds a message into the running one
///   (P6); `turn/interrupt {turnId}` ends it at once (P7). `turn/started` and `turn/completed`
///   bracket it. A `commandExecution` item names its process, which app-server leaves running
///   after an interrupt (P7), so the driver kills it.
/// - Approvals are **declined, never cancelled** (P5): a decline is read by the model as "rejected
///   by user" and the turn goes on; a cancel ends the turn `interrupted`.
/// - The delta notifications marion reads nothing from are opted out of (P10).
pub const APP: RpcChannel = RpcChannel {
    initialize: "initialize",
    initialized: Some("initialized"),
    opt_out: &[
        "item/agentMessage/delta",
        "item/commandExecution/outputDelta",
        "account/rateLimits/updated",
        "turn/diff/updated",
        "remoteControl/status/changed",
    ],
    open: "thread/start",
    resume: "thread/resume",
    resume_id: "threadId",
    cwd: "cwd",
    open_fields: &[("ephemeral", "false"), ("sandbox", "\"workspace-write\"")],
    // The same mode the row's argv pair names (`read_only`), on the request that beats it.
    read_only_fields: &[("sandbox", READ_ONLY_SANDBOX)],
    thread_id: "/result/thread/id",
    ready: Some(ReadyGate {
        at: &[Cond::Eq("/method", "mcpServer/startupStatus/updated")],
        server: "/params/name",
        status: "/params/status",
        ready: "ready",
        failed: &["failed", "cancelled"],
        error: "/params/error",
    }),
    turns: Turns {
        start: "turn/start",
        steer: "turn/steer",
        interrupt: "turn/interrupt",
        thread: "threadId",
        input: "input",
        text_item: ("type", "text", "text"),
        expected: "expectedTurnId",
        turn: "turnId",
        started: &[Cond::Eq("/method", "turn/started")],
        ended: &[Cond::Eq("/method", "turn/completed")],
        turn_id: "/params/turn/id",
        answered: &["/result/turn/id", "/result/turnId"],
        process: &[
            Cond::Eq("/method", "item/started"),
            Cond::Eq("/params/item/type", "commandExecution"),
        ],
        process_ended: &[
            Cond::Eq("/method", "item/completed"),
            Cond::Eq("/params/item/type", "commandExecution"),
        ],
        pid: "/params/item/processId",
    },
    answers: &[
        Answer {
            method: "item/commandExecution/requestApproval",
            result: r#"{"decision":"decline"}"#,
        },
        Answer {
            method: "item/fileChange/requestApproval",
            result: r#"{"decision":"decline"}"#,
        },
        Answer {
            method: "mcpServer/elicitation/request",
            result: r#"{"action":"decline","content":null,"_meta":null}"#,
        },
    ],
    note: "S36 on codex 0.155.1 (tests/fixtures/app-server-0.155.1): P2 handshake, P3 readiness \
           and sandbox, P5 approvals, P6 steer, P7 interrupt, P8 resume, P10 opt-out",
};

/// How a `codex app-server` stream is read: [`STREAM`]'s readings over app-server's notifications
/// (S36 P4, `p4-items.jsonl`). Items arrive as `item/started` then `item/completed` under one
/// `params.item.id`, camel-cased (`mcpToolCall`, `commandExecution`, `fileChange`,
/// `agentMessage`); an item may complete **after** its turn did (P7), and is read all the same.
///
/// - **Usage** is `thread/tokenUsage/updated`'s `total`, the thread's running sum after each
///   provider response, so it folds as the session total `exec`'s `turn.completed` is.
///   `inputTokens` counts `cachedInputTokens` as it does there.
/// - **The session** is the thread id in the answer to `thread/start` or `thread/resume`, which
///   names the thread it reopened (P8), so a different id is a fresh thread.
/// - **Failures** are a `turn/completed` whose turn `failed`, and the `error` notification.
pub const APP_STREAM: StreamGrammar = StreamGrammar {
    call: Where {
        frame: &[
            Cond::Eq("/params/item/type", "mcpToolCall"),
            Cond::Eq("/params/item/server", MCP_ALIAS),
        ],
        each: None,
        unit: &[],
    },
    name: Name::Verb("/params/item/tool"),
    args: "/params/item/arguments",
    pairing: Pairing::SameUnit {
        id: Some("/params/item/id"),
        verdict: Verdict::Status {
            path: "/params/item/status",
            ok: "completed",
            pending: &["inProgress"],
            words: &["/params/item/error/message"],
        },
    },
    refused_report: OnRefusedReport::Record,
    failures: &[crate::grammar::Failure::Frame {
        at: Where {
            frame: &[
                Cond::Eq("/method", "turn/completed"),
                Cond::Eq("/params/turn/status", "failed"),
            ],
            each: None,
            unit: &[],
        },
        words: &["/params/turn/error/message"],
        fallback: "the child's turn/completed said the turn failed",
    }],
    errors: &[
        ErrorRule {
            at: Where {
                frame: &[Cond::Eq("/method", "error")],
                each: None,
                unit: &[],
            },
            status: None,
            kind: Some("/params/error/codexErrorInfo"),
            // The provider's own words ride `additionalDetails` (`unexpected status 401 …`), the
            // retry count `message` (`Reconnecting... 1/5`): conformance P-errors on 0.155.1.
            words: &["/params/error/additionalDetails", "/params/error/message"],
        },
        ErrorRule {
            at: Where {
                frame: &[
                    Cond::Eq("/method", "turn/completed"),
                    Cond::Eq("/params/turn/status", "failed"),
                ],
                each: None,
                unit: &[],
            },
            status: None,
            kind: Some("/params/turn/error/codexErrorInfo"),
            words: &["/params/turn/error/message"],
        },
    ],
    file_changes: Some(PathList {
        at: Where {
            frame: &[
                Cond::Eq("/method", "item/completed"),
                Cond::Eq("/params/item/type", "fileChange"),
            ],
            each: None,
            unit: &[],
        },
        list: "/params/item/changes",
        path: "/path",
    }),
    // No S36 frame was read for the running model; the exec row's measurement is not this one.
    model: None,
    session: Some(SessionId {
        at: Where {
            frame: &[Cond::Has("/result/thread/id")],
            each: None,
            unit: &[],
        },
        path: "/result/thread/id",
        resumes_in_place: true,
        // `thread/start` answers the thread's id before the first turn.
        by_title: None,
    }),
    usage: Some(UsageRule {
        at: Where {
            frame: &[Cond::Eq("/method", "thread/tokenUsage/updated")],
            each: None,
            unit: &[],
        },
        input: "/params/tokenUsage/total/inputTokens",
        output: "/params/tokenUsage/total/outputTokens",
        cache_read: Some("/params/tokenUsage/total/cachedInputTokens"),
        cache_write: Some("/params/tokenUsage/total/cacheWriteInputTokens"),
        reasoning: Some(Reasoning::Within(
            "/params/tokenUsage/total/reasoningOutputTokens",
        )),
        input_includes_cache: true,
        fold: UsageFold::Session,
        // The thread's running total follows every response, so a turn cut short has already
        // been counted up to its last one.
        in_flight: None,
    }),
    rate_limit: None,
    activity: Some(ActivityRule {
        calls: &[
            ToolUnit {
                at: Where {
                    frame: &[Cond::Eq("/params/item/type", "mcpToolCall")],
                    each: None,
                    unit: &[],
                },
                name: "/params/item/tool",
                args: "/params/item/arguments",
                id: Some("/params/item/id"),
                shape: CallShape::Tool,
                end: Some(APP_ITEM_END),
            },
            ToolUnit {
                at: Where {
                    frame: &[Cond::Eq("/params/item/type", "commandExecution")],
                    each: None,
                    unit: &[],
                },
                name: "/params/item/type",
                args: "/params/item/command",
                id: Some("/params/item/id"),
                shape: CallShape::Command,
                end: Some(APP_COMMAND_END),
            },
            ToolUnit {
                at: Where {
                    frame: &[Cond::Eq("/params/item/type", "fileChange")],
                    each: None,
                    unit: &[],
                },
                name: "/params/item/type",
                args: "/params/item/changes",
                id: Some("/params/item/id"),
                shape: CallShape::Files,
                end: Some(APP_ITEM_END),
            },
        ],
        text: &[TextUnit {
            at: Where {
                frame: &[
                    Cond::Eq("/method", "item/completed"),
                    Cond::Eq("/params/item/type", "agentMessage"),
                ],
                each: None,
                unit: &[],
            },
            path: "/params/item/text",
            joins: false,
        }],
    }),
};

/// How an app-server item ends: its `status` on `item/completed` (S36 P4/P5: `completed`, `failed`,
/// and `declined` for a command marion's answer declined).
const APP_ITEM_END: CallEnd = CallEnd {
    status: "/params/item/status",
    ok: &["completed"],
    failed: &["failed", "declined"],
    exit: None,
};

/// [`APP_ITEM_END`] for a `commandExecution` item, which also states its `exitCode`.
const APP_COMMAND_END: CallEnd = CallEnd {
    exit: Some("/params/item/exitCode"),
    ..APP_ITEM_END
};

/// The sandbox every codex node marion generates a config for runs in — and **this harness's whole
/// availability axis** (§3.1). `codex exec` has no `--tools` and no permission list; it exposes
/// only `--sandbox <read-only|workspace-write|danger-full-access>`, so this one value is what
/// decides whether a codex child can change a file at all.
///
/// Named rather than inlined into [`config_toml`] because
/// `crate::adapter::CodexAdapter::tool_name` maps marion's `write` onto it as §3.1's *"coarsest
/// equivalent"* (`sandbox:workspace-write`): two spellings of one grant could drift, and then the
/// adapter would be reporting a mode the generated config does not set.
/// The variable the generated provider's `env_key` names — [`config_toml`] spells it literally,
/// and `a_canned_codex_node_carries_its_credential_in_the_variable_its_config_names` holds the two
/// together.
pub const PROVIDER_KEY_ENV: &str = "MARION_PROVIDER_KEY";

pub const SANDBOX_MODE: &str = "workspace-write";

/// The config key [`SANDBOX_MODE`] is set under, in the generated `config.toml` and on a live
/// node's `-c` channel alike.
pub const SANDBOX_KEY: &str = "sandbox_mode";

/// The sandbox a **read-only** node runs in ([`SPEC`]'s `read_only`), TOML-quoted as it rides
/// `-c`: the one codex mode that refuses `apply_patch` and the shell's writes alike (s38).
pub const READ_ONLY_SANDBOX: &str = "\"read-only\"";

/// [`SANDBOX_MODE`] as the `-c` pair a **live** node carries — the constraint the contract records,
/// compiled onto the one launch that reads no marion-written config.
///
/// Without it a live node runs in whatever the operator's `~/.codex/config.toml` resolves for the
/// worktree, and an untrusted worktree resolves to `read-only`: measured on 0.155.1 (2026-09-22),
/// `turn_context.sandbox_policy` read `read-only` on a node whose contract said `workspace-write`,
/// and `-c sandbox_mode="workspace-write"` on the same untrusted directory read `workspace-write`.
/// Separate from [`live_config_overrides`] because it is not part of marion's MCP declaration: it
/// is owed with or without a bridge, and a native facade session (the operator's own codex) keeps
/// the operator's sandbox.
pub fn live_sandbox_override() -> (String, String) {
    (SANDBOX_KEY.to_string(), toml_str(SANDBOX_MODE))
}

/// **The `-c` pair that keeps [`live_sandbox_override`] from trusting the operator's repository.**
///
/// Measured on 0.155.1 (2026-09-28, scratch `CODEX_HOME`): `codex exec` asked for
/// `workspace-write` in a project with no `trust_level` writes `[projects."<main checkout>"]
/// trust_level = "trusted"` into the `config.toml` it read — one entry per repository, keyed on
/// the main checkout even from a worktree — while `read-only` writes nothing. Codex persists only
/// where the project's trust is unset (`thread_start_task`), and a `-c projects={…}` inline table
/// counts as set (the dotted `projects."<p>".trust_level` spelling does not reach it): with
/// `trust_level = "untrusted"` there the run still resolves `workspace-write` and nothing is
/// written. `untrusted` is what an unset trust already means, so the run is the one codex would
/// have made — marion only declines to record a trust the operator never gave.
///
/// `None` — nothing to add — outside git, or where the operator's `config.toml` (`theirs`)
/// already states a trust for the checkout: there codex writes nothing either, and their choice
/// stands.
pub fn live_trust_override(cwd: &Path, theirs: Option<&str>) -> Option<(String, String)> {
    let checkout = main_checkout(cwd)?;
    let key = checkout.to_string_lossy().into_owned();
    let stated = theirs
        .and_then(|t| t.parse::<toml::Table>().ok())
        .and_then(|t| t.get("projects")?.get(&key)?.get("trust_level").cloned())
        .is_some();
    (!stated).then(|| {
        (
            "projects".to_string(),
            format!("{{{}={{trust_level=\"untrusted\"}}}}", toml_str(&key)),
        )
    })
}

/// The main checkout of the git repository `cwd` is in — codex's key for a project's trust —
/// read off the `.git` entry rather than by running git: the checkout that holds `.git` as a
/// directory, or, from a linked worktree's `.git` file, the parent of its common dir.
pub fn main_checkout(cwd: &Path) -> Option<PathBuf> {
    let cwd = cwd.canonicalize().ok()?;
    let top = cwd.ancestors().find(|d| d.join(".git").exists())?;
    let dot = top.join(".git");
    if dot.is_dir() {
        return Some(top.to_path_buf());
    }
    let text = std::fs::read_to_string(&dot).ok()?;
    let gitdir = top.join(text.strip_prefix("gitdir:")?.trim());
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(rel) => gitdir.join(rel.trim()),
        Err(_) => gitdir,
    };
    common.canonicalize().ok()?.parent().map(Path::to_path_buf)
}

/// TOML basic-string escaping, for the handful of characters a path may legally contain.
///
/// Not decorative. Every value below is a filesystem path or a URL that marion did not author — an
/// unescaped `"` or `\` would produce a `config.toml` codex cannot parse, and a config it cannot
/// parse fails as a launch error that names the file rather than the value.
fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `$CODEX_HOME/config.toml` marion writes for a codex node.
///
/// **`default_tools_approval_mode = "approve"` is load-bearing**: without it every marion tool
/// call is silently cancelled — no error the child can act on, and the run ends `Unreported`.
///
/// **The `env` block is load-bearing too, and its absence was silent in exactly the same way.**
/// Until it existed this function took only `(bridge, bridge_args, base_url)` and emitted no `env`
/// at all, so a codex node's bridge inherited marion's environment and found none of the five
/// variables it reads. A codex *child* survived that, because its one call is `report` and `report`
/// reads nothing; a codex **root** did not — its `spawn` call returned
/// `marion: MARION_REPO is not set` as a tool error, and any contract that did get written would
/// have been stamped `requester = "unattributed-root"` (`marion-supervisor::main::requester`),
/// which §6.7 makes an audit record naming an agent-dir that does not exist. codex's TOML schema
/// has always supported `env` inside `[mcp_servers.<name>]`; nothing was blocking this but the
/// plumbing, and `ctx.agent_id` was already threaded to the call site.
/// The MCP server alias: the `marion` of `[mcp_servers.marion]`, and the `server` an
/// `mcp_tool_call` item names ([`STREAM`]). One spelling for the document and the reader.
pub const MCP_ALIAS: &str = crate::spec::MCP_ALIAS;

/// The config key `[mcp_servers.marion]` sits at, as a dotted path.
///
/// Doubles as the needle [`crate::McpRoute::Argv`] hands the supervisor: an argv that does not
/// mention this key carries no declaration, whatever else it carries.
pub const MCP_SERVER_KEY: &str = "mcp_servers.marion";

/// The feature flag whose default `true` leaves a `git fetch` running after `exec` has exited.
///
/// Verified against the installed 0.146.0: `codex features list` reports `plugins  stable  true`
/// by default and `plugins  stable  false` under `-c features.plugins=false`. `--disable plugins`
/// is documented as the exact equivalent (`-c features.<name>=false`); the dotted form is emitted
/// because it is the same key path the generated `config.toml` has always written, so the two
/// routes cannot drift into disabling different things.
pub const PLUGINS_FEATURE_KEY: &str = "features.plugins";

/// The whole of marion's configuration for a **live** codex node, as `-c key=value` pairs.
///
/// **Why argv and not a file.** Under [`Auth::Inherited`] `CODEX_HOME` is unset, so
/// `$CODEX_HOME/config.toml` *is* `~/.codex/config.toml` — the operator's own. §6.4's central MUST
/// is that marion never mutates it, and there is no third location: `-p/--profile` also resolves
/// under `$CODEX_HOME`, and `--ignore-user-config` would throw away the operator's provider and
/// model defaults along with everything else. `-c` is the one channel that overlays without
/// writing. Values are TOML-parsed by codex (falling back to a literal string), which is why every
/// string below goes through [`toml_str`] rather than being interpolated raw.
///
/// **Every key here was checked against the installed binary rather than assumed**, because a
/// silently-ignored `-c` key is this project's recurring failure shape — and codex *does* accept
/// unknown keys without complaint (`-c mcp_servers.marion.totally_bogus_key="x"` is a clean exit).
/// `codex mcp get marion --json` with these overrides renders the server, its `args` and its `env`
/// map; `mcp_servers.marion.default_tools_approval_mode="not-a-mode"` is rejected with
/// *"unknown variant `not-a-mode`, expected one of `auto`, `prompt`, `writes`, `approve`"*, which is
/// the proof that the key is really parsed and that `approve` is really one of its values.
///
/// The `env` block comes from [`BridgeEnv::pairs`], shared with [`config_toml`], so the live route
/// and the canned one cannot hand the bridge different sets of variables — **except the node
/// token**, which `ps` would show to every user on argv. `env` is the declared bridge, which the
/// row's live carrier has already stripped of it; the token rides codex's own environment instead,
/// and `env_vars` names its variable so codex passes it on to the bridge.
pub fn live_config_overrides(env: &BridgeEnv) -> Vec<(String, String)> {
    let args = env
        .args
        .iter()
        .map(|a| toml_str(a))
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = vec![
        (
            format!("{MCP_SERVER_KEY}.command"),
            toml_str(&env.bridge.to_string_lossy()),
        ),
        (format!("{MCP_SERVER_KEY}.args"), format!("[{args}]")),
        // **Load-bearing** (§12): without it every marion tool call is silently cancelled and the
        // bridge never receives `tools/call` — no error the node can act on, and a run that ends
        // `Unreported`. It is the trap that would have made S6 answer its own question wrong.
        (
            format!("{MCP_SERVER_KEY}.default_tools_approval_mode"),
            toml_str("approve"),
        ),
        // Measured 2026-09-22 on 0.155.1: codex defers MCP tools behind `tool_search` unless the
        // server is omitted from that surface, and a live child that must search for `report`
        // did not call it. Parsed by 0.147.0 and later; 0.146.0 ignores it as an unknown key.
        (
            format!("{MCP_SERVER_KEY}.omit_tools_from"),
            format!("[{}]", toml_str("deferred")),
        ),
    ];
    for (k, v) in env.pairs() {
        out.push((format!("{MCP_SERVER_KEY}.env.{k}"), toml_str(&v)));
    }
    let forwarded = SPEC.token.live.forwarded();
    if !forwarded.is_empty() {
        let names = forwarded
            .iter()
            .map(|n| toml_str(n))
            .collect::<Vec<_>>()
            .join(", ");
        out.push((format!("{MCP_SERVER_KEY}.env_vars"), format!("[{names}]")));
    }
    // Measured (S7 / §12): left on, `codex exec` starts a curated-plugin-marketplace clone whose
    // `git fetch` OUTLIVES the process, reparents to pid 1, writes into the agent dir after
    // teardown, and reaches the network on a run specified to make none. It is not reapable after
    // the fact — §9's kill rule turns on enumerating descendants *before* the child dies — so
    // prevention at config time is the only remedy, on this route exactly as on the other.
    out.push((PLUGINS_FEATURE_KEY.to_string(), "false".to_string()));
    out
}

pub fn config_toml(env: &BridgeEnv, base_url: &str) -> String {
    let args = env
        .args
        .iter()
        .map(|a| toml_str(a))
        .collect::<Vec<_>>()
        .join(", ");
    let env_table = env
        .pairs()
        .iter()
        .map(|(k, v)| format!("{k} = {}", toml_str(v)))
        .collect::<Vec<_>>()
        .join(", ");
    let bridge = toml_str(&env.bridge.to_string_lossy());
    format!(
        r#"model_provider = "canned"
approval_policy = "never"
{SANDBOX_KEY} = "{SANDBOX_MODE}"

# Measured on 0.146.0: `codex exec` starts a **background** `git fetch` of the curated plugin
# marketplace into `$CODEX_HOME/.tmp/plugins-clone-*`, and it OUTLIVES the exec process. marion
# deletes the agent-dir with the node (§6.4), so that fetch would keep writing into a directory
# that is being removed — and, worse, it is precisely the untracked runaway §9's kill rule exists
# to prevent: by the time `exec` has exited its descendants have reparented to pid 1 and no
# ancestry walk can find them. It also reaches the network on a run whose whole point is that it
# does not. There is nothing for marion to reap here, so the fix is to never start it.
[features]
plugins = false

[model_providers.canned]
name = "canned"
base_url = "{base_url}"
wire_api = "responses"
env_key = "MARION_PROVIDER_KEY"

[mcp_servers.marion]
command = {bridge}
args = [{args}]
default_tools_approval_mode = "approve"
omit_tools_from = ["deferred"]
env = {{ {env_table} }}
"#
    )
}

/// Codex 0.146.0, `exec` surface (§9). marion's M1 child.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexAdapter;

impl CodexAdapter {
    fn config_path(spec: &LaunchSpec) -> PathBuf {
        spec.config_dir.join("config.toml")
    }
}

impl HarnessAdapter for CodexAdapter {
    fn harness(&self) -> Harness {
        Harness::Codex
    }

    /// **Under `Inherited` the declaration is compiled into argv, not written to a file** — and
    /// `CODEX_HOME` is dropped, which is what makes the login visible in the first place. The two go
    /// together: unsetting the variable points codex at the operator's `~/.codex/auth.json` *and* at
    /// the operator's `~/.codex/config.toml`, and marion is forbidden to write the second (§6.4), so
    /// the `-c` overlay is the only channel left. The row's `Each("-c", Pairs)` carries it, and the
    /// `Canned`-gated `CODEX_HOME` row drops the variable.
    ///
    /// The same fields serve both shapes: `SPEC`'s pane row simply names no `--output-schema`
    /// or `--output-last-message`, so the two `exec`-only outputs are ignored there structurally
    /// rather than by a second branch that sets them `None`.
    fn fields(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
        _shape: spec::Shape,
    ) -> Result<spec::Fields, HarnessError> {
        // The trait's default `axes` runs the refusal owed to a `tools:` declaration on every
        // harness; nothing in the row reads the result — see `Self::tool_name` for why a codex
        // declaration compiles no flag. Discarding the names is the honest outcome.
        let mut f = self.launch_fields(spec, ctx)?;
        // A live node reads no marion-written config, so the sandbox the contract records rides
        // `-c` first, bridge or no bridge (`live_sandbox_override`).
        f.pairs = match (spec.auth, spec.mcp) {
            (Auth::Canned | Auth::Endpoint, _) => Vec::new(),
            (Auth::Inherited, McpDeclaration::None) => vec![live_sandbox_override()],
            (Auth::Inherited, McpDeclaration::Marion) => std::iter::once(live_sandbox_override())
                .chain(live_config_overrides(&declared_bridge(self, spec, ctx)))
                .collect(),
        };
        // ... and the sandbox must not record a trust the operator never gave in their own
        // `config.toml` (`live_trust_override`), read here and never written.
        if spec.auth == Auth::Inherited {
            let theirs = spec
                .extra
                .profile_dir
                .clone()
                .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from))
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
                .and_then(|d| std::fs::read_to_string(d.join("config.toml")).ok());
            f.pairs
                .extend(live_trust_override(&spec.cwd, theirs.as_deref()));
        }
        Ok(f)
    }

    fn config_files(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError> {
        // **No file at all under `Inherited`, and that is the MUST rather than a convenience.**
        // With `CODEX_HOME` unset the only `config.toml` codex reads is `~/.codex/config.toml` —
        // the operator's own — and §6.4 forbids marion mutating it. Writing marion's document
        // *anywhere else* would simply not be read, and writing it there would clobber a login
        // marion is not even running. So the declaration moves to argv (see `compile` /
        // `mcp_route`) and this route emits nothing.
        if spec.auth == Auth::Inherited {
            // No `base_url` is demanded either: the refusal below exists because a *generated*
            // `model_providers` block pointing nowhere is an unbounded hang, and a live node
            // generates none — it uses codex's own default provider and the operator's credential.
            return Ok(Vec::new());
        }
        let base_url = spec.base_url.as_deref().ok_or(HarnessError::MissingInput {
            harness: Harness::Codex,
            what: "a model_providers entry needs a base_url; a config pointing nowhere fails as \
                   a hang, which is the worst failure to diagnose",
        })?;
        // The bridge's identity reaches a codex node's `[mcp_servers.marion]` `env` — a codex
        // **root** without it answered `spawn` with `marion: MARION_REPO is not set`. Written on
        // every canned node, declaration or not: the document is the whole of the node's config.
        let bridge = declared_bridge(self, spec, ctx);
        Ok(vec![(
            Self::config_path(spec),
            config_toml(&bridge, base_url),
        )])
    }
}

/// This row's entry in [`crate::adapter::ROWS`].
pub const ROW: Row = Row {
    spec: &SPEC,
    adapter: |_| Ok(Box::new(CodexAdapter)),
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use marion_core::contract::AgentId;
    use serde_json::Value;

    use crate::auth::Auth;
    use crate::invocation::Invocation;
    use crate::spec::{Fields, Shape, render};

    /// A canned node as the adapter's fields hook shapes it: no model (a canned launch compiles
    /// none whatever was asked for) and no `-c` pairs (the same settings are in the generated
    /// `config.toml`). The hook's decisions are pinned in `adapter::tests`; these pin **the row**.
    fn spec() -> Fields {
        Fields {
            cwd: "/tmp/wt".into(),
            config_dir: "/tmp/ch".into(),
            auth: Auth::Canned,
            prompt: "do the task".into(),
            model: None,
            ..Fields::default()
        }
    }

    /// What `--live` hands a codex node: no `CODEX_HOME`, and the declaration on `-c` flags.
    fn live_spec() -> Fields {
        Fields {
            auth: Auth::Inherited,
            pairs: live_config_overrides(&BridgeEnv {
                auth: Auth::Inherited,
                base_url: None,
                ..bridge_env()
            }),
            ..spec()
        }
    }

    fn compile_exec(f: &Fields) -> Invocation {
        render(&EXEC, Shape::Headless, f).unwrap()
    }

    fn compile_app_server(f: &Fields) -> Invocation {
        render(&SPEC, Shape::Headless, f).unwrap()
    }

    fn compile_tui(f: &Fields) -> Invocation {
        render(&SPEC, Shape::Pane, f).unwrap()
    }

    /// The `-c` pairs an invocation carries, in order.
    fn pairs(inv: &Invocation) -> Vec<String> {
        inv.args
            .windows(2)
            .filter(|w| w[0] == "-c")
            .map(|w| w[1].clone())
            .collect()
    }

    /// **A live node's sandbox does not trust the operator's repository for them.** Unset, the
    /// override pins the main checkout — also from a linked worktree — to `untrusted` in memory;
    /// where their `config.toml` states any trust for it, or outside git, nothing is added.
    #[test]
    fn a_live_sandbox_pins_an_unstated_trust_to_untrusted_and_leaves_a_stated_one() {
        let dir = marion_testsupport::scratch("codex-trust-override");
        let main = dir.join("main");
        let wt = dir.join("wt");
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        let (m, w) = (main.to_string_lossy(), wt.to_string_lossy());
        git(&["init", "-q", &m]);
        git(&["-C", &m, "commit", "-q", "--allow-empty", "-m", "i"]);
        git(&["-C", &m, "worktree", "add", "-q", &w]);
        let canon = main.canonicalize().unwrap();
        assert_eq!(main_checkout(&wt).as_deref(), Some(canon.as_path()));
        assert_eq!(main_checkout(&main).as_deref(), Some(canon.as_path()));

        let key = canon.to_string_lossy().into_owned();
        let (k, v) = live_trust_override(&wt, None).expect("unset trust is pinned");
        assert_eq!(k, "projects");
        assert_eq!(
            v,
            format!("{{{}={{trust_level=\"untrusted\"}}}}", toml_str(&key))
        );
        // What codex parses the pair into: the checkout, untrusted, and nothing else.
        let parsed: toml::Table = format!("{k}={v}").parse().unwrap();
        assert_eq!(
            parsed["projects"][key.as_str()]["trust_level"].as_str(),
            Some("untrusted")
        );
        let other = "[projects.\"/elsewhere\"]\ntrust_level = \"trusted\"\n";
        assert!(live_trust_override(&wt, Some(other)).is_some());
        for stated in ["trusted", "untrusted"] {
            let theirs = format!(
                "[projects.{}]\ntrust_level = \"{stated}\"\n",
                toml_str(&key)
            );
            assert_eq!(live_trust_override(&wt, Some(&theirs)), None, "{stated}");
        }
        let plain = dir.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(live_trust_override(&plain, None), None, "outside git");
    }

    /// **A failed turn is the stream's failure claim, in codex's own words** — measured on 0.155.1
    /// against the canned provider refusing the key: a `turn.failed` frame closes the run with
    /// `error.message`. The informational `item.completed` of type `error` (missing model
    /// metadata) is not a failure.
    #[test]
    fn a_failed_turn_is_a_failure_claim_and_an_error_item_is_not() {
        let stdout = concat!(
            r#"{"type":"thread.started","thread_id":"t-1"}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"item_0","type":"error","message":"Model metadata for `m` not found."}}"#,
            "\n",
            r#"{"type":"turn.started"}"#,
            "\n",
            r#"{"type":"error","message":"exceeded retry limit, last status: 429 Too Many Requests"}"#,
            "\n",
            r#"{"type":"turn.failed","error":{"message":"exceeded retry limit, last status: 429 Too Many Requests"}}"#,
            "\n",
        );
        let out = crate::grammar::parse_stream(&STREAM, stdout, "");
        assert_eq!(
            out.failure.as_deref(),
            Some("exceeded retry limit, last status: 429 Too Many Requests")
        );
        let clean = concat!(
            r#"{"type":"item.completed","item":{"id":"item_0","type":"error","message":"Model metadata for `m` not found."}}"#,
            "\n"
        );
        assert_eq!(
            crate::grammar::parse_stream(&STREAM, clean, "").failure,
            None
        );
    }

    #[test]
    fn exec_is_json_and_the_prompt_is_positional() {
        let inv = compile_exec(&spec());
        assert_eq!(inv.args[0], "exec");
        assert!(inv.args.contains(&"--json".to_string()));
        assert_eq!(inv.args.last().unwrap(), "do the task");
    }

    #[test]
    fn codex_home_is_set_in_env_not_argv() {
        let inv = compile_exec(&spec());
        assert!(
            inv.env
                .iter()
                .any(|(k, v)| k == "CODEX_HOME" && v == "/tmp/ch")
        );
        assert!(!inv.args.iter().any(|a| a.contains("CODEX_HOME")));
    }

    /// **`-C` is an `exec` flag that `exec resume` does not take.** Measured on 0.147.0
    /// (`tests/fixtures/s29/`): `codex exec resume --help` lists no `-C/--cd`, and
    /// `exec resume <id> … -C <dir>` exits 2 with *"unexpected argument '-C' found"* — the exit the
    /// demo's `marion resume` relaunch died with. `codex exec [OPTIONS] <COMMAND> [ARGS]` takes it
    /// ahead of the subcommand, so the row places `-C` before `Arg::Resume`; the rest of the flags
    /// are listed by both helps and were probed accepted after `resume <id>`. Pinned as the whole
    /// vector so a reordering has to re-measure.
    #[test]
    fn a_resume_places_the_working_root_ahead_of_the_subcommand_that_does_not_take_it() {
        let inv = compile_exec(&Fields {
            resume: Some("019a-thread".into()),
            ..spec()
        });
        assert_eq!(
            inv.args,
            [
                "exec",
                "-C",
                "/tmp/wt",
                "resume",
                "019a-thread",
                "--json",
                "--skip-git-repo-check",
                // The row's update policy, on the `-c` channel both helps list.
                "-c",
                "check_for_update_on_startup=false",
                "do the task",
            ]
        );
        let fresh = compile_exec(&spec());
        assert_eq!(
            fresh.args,
            [
                "exec",
                "-C",
                "/tmp/wt",
                "--json",
                "--skip-git-repo-check",
                "-c",
                "check_for_update_on_startup=false",
                "do the task"
            ],
            "a fresh launch is the same row without the subcommand"
        );
    }

    /// **A live resume re-sends marion's MCP declaration, after the subcommand.** S31
    /// (`tests/fixtures/s31-turn-delivery/p0b/codex/`): `exec resume <id>` without the `-c
    /// mcp_servers.marion.*` pairs continues the thread and never starts marion's server — no
    /// `initialize` reaches it — while the history still names marion's tools, so the resumed
    /// turn's first marion call would fail. With the pairs the server is spawned again. So every
    /// pair a fresh live launch carries must ride the resume too, and after `resume <id>`, where
    /// the row's `Arg::Each("-c")` renders them.
    #[test]
    fn a_live_resume_redeclares_marions_mcp_server_after_the_subcommand() {
        let fresh = compile_exec(&live_spec());
        let resumed = compile_exec(&Fields {
            resume: Some("019a-thread".into()),
            ..live_spec()
        });
        let pairs = |args: &[String]| -> Vec<String> {
            args.windows(2)
                .filter(|w| w[0] == "-c" && w[1].starts_with(MCP_SERVER_KEY))
                .map(|w| w[1].clone())
                .collect()
        };
        assert!(
            !pairs(&fresh.args).is_empty(),
            "the live row declares marion on -c"
        );
        assert_eq!(pairs(&resumed.args), pairs(&fresh.args));
        let resume = resumed
            .args
            .iter()
            .position(|a| a == "resume")
            .expect("the subcommand");
        let first_pair = resumed
            .args
            .iter()
            .position(|a| a.starts_with(MCP_SERVER_KEY))
            .expect("a pair");
        assert!(
            resume < first_pair,
            "the declaration follows `resume <id>`: {:?}",
            resumed.args
        );
    }

    /// **The interactive command is not `exec` with a flag off.** Every name below is an `exec`
    /// flag that `codex --help` on 0.147.0 does not list, so compiling one here is an argv the
    /// binary rejects before it draws a cell — a pane that fails at launch rather than a pane that
    /// renders wrong, which is the harder failure to attribute.
    ///
    /// Mutation: make `compile_tui` delegate to `compile_exec`. This fails on `exec` itself.
    #[test]
    fn the_tui_carries_none_of_the_exec_shapes_protocol_argv() {
        let inv = compile_tui(&spec());
        for forbidden in [
            "exec",
            "--json",
            "--skip-git-repo-check",
            "--output-schema",
            "--output-last-message",
        ] {
            assert!(
                !inv.args.iter().any(|a| a == forbidden),
                "the TUI argv carries {forbidden}, which the interactive command does not accept: \
                 {:?}",
                inv.args
            );
        }
        assert_eq!(inv.program, "codex");
    }

    /// The two `exec`-only outputs are **ignored rather than compiled**, and a caller that sets one
    /// finds out here rather than by watching a flag vanish.
    #[test]
    fn the_execs_output_files_are_ignored_on_the_tui_rather_than_silently_dropped_into_argv() {
        let inv = compile_tui(&Fields {
            output_schema: Some("/tmp/schema.json".into()),
            output_last_message: Some("/tmp/last.txt".into()),
            ..spec()
        });
        assert!(
            !inv.args
                .iter()
                .any(|a| a.contains("schema") || a.contains("last")),
            "an exec-only output path reached the interactive argv: {:?}",
            inv.args
        );
    }

    /// The prompt is a positional and an empty one compiles none — a TUI opened at its composer.
    ///
    /// **It is submitted, not seeded**, which is where this harness differs from Claude Code's
    /// pane; see [`compile_tui`].
    #[test]
    fn the_tui_prompt_is_the_last_positional_and_an_empty_one_is_no_positional() {
        let inv = compile_tui(&spec());
        assert_eq!(inv.args.last().map(String::as_str), Some("do the task"));
        let bare = compile_tui(&Fields {
            prompt: String::new(),
            ..spec()
        });
        assert!(
            !bare.args.iter().any(String::is_empty),
            "an empty prompt compiled an empty positional: {:?}",
            bare.args
        );
        assert_eq!(bare.args.last().map(String::as_str), Some("/tmp/wt"));
    }

    /// The workspace is stated on argv as well as being the process cwd: `-C` is what codex scopes
    /// its sandbox to, and leaving it out would make the agent's working root a fact about whoever
    /// launched the supervisor.
    #[test]
    fn the_tui_names_its_working_root_rather_than_inheriting_it() {
        let inv = compile_tui(&spec());
        let i = inv.args.iter().position(|a| a == "-C").expect("-C");
        assert_eq!(inv.args[i + 1], "/tmp/wt");
        assert_eq!(inv.cwd, PathBuf::from("/tmp/wt"));
    }

    /// Isolation is the `exec` shape's, exactly — present under canned, **absent** rather than
    /// empty under inherited, for [`SPEC`]'s `CODEX_HOME` row's reason.
    #[test]
    fn the_tui_is_isolated_by_codex_home_and_a_live_one_is_not_isolated_at_all() {
        let inv = compile_tui(&spec());
        assert!(
            inv.env
                .iter()
                .any(|(k, v)| k == "CODEX_HOME" && v == "/tmp/ch"),
            "a paned codex would read and write the operator's own ~/.codex"
        );
        let live = compile_tui(&live_spec());
        assert!(!live.env.iter().any(|(k, _)| k == "CODEX_HOME"));
    }

    /// The live route's `-c` overrides ride the TUI's argv in the same order and the same spelling
    /// they ride `exec`'s, because there is one row field behind both.
    #[test]
    fn the_tui_carries_the_same_config_overrides_the_exec_shape_does() {
        let tui = compile_tui(&live_spec());
        let exec = compile_exec(&live_spec());
        assert!(!pairs(&tui).is_empty(), "a live TUI carries no declaration");
        assert_eq!(pairs(&tui), pairs(&exec));
    }

    fn bridge_env() -> BridgeEnv {
        BridgeEnv {
            node_token_file: None,
            bridge: "/bin/marion-supervisor".into(),
            args: vec!["mcp".into()],
            repo: "/repo".into(),
            state: "/state".into(),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
            agent_id: AgentId("019f-node".into()),
            agent_type: "codex-impl".into(),
            depth: 1,
            node_token: None,
            ready_file: None,
        }
    }

    #[test]
    fn the_approval_mode_that_silently_cancels_everything_is_always_written() {
        let t = config_toml(&bridge_env(), "http://127.0.0.1:8099/v1");
        assert!(
            t.contains(r#"default_tools_approval_mode = "approve""#),
            "without it every marion tool call is cancelled with no error the child can see"
        );
    }

    /// Measured on 0.146.0 through the M1 end-to-end run: with the plugin feature left on, every
    /// `codex exec` leaves a `git fetch https://github.com/openai/plugins.git` running **after it
    /// has exited**, reparented to pid 1, writing into the agent-dir marion is deleting. It is not
    /// reapable after the fact — §9's kill rule turns on enumerating descendants *before* the
    /// child dies — so the only remedy is not to start it.
    #[test]
    fn the_background_plugin_fetch_that_outlives_exec_is_disabled() {
        let t = config_toml(&bridge_env(), "http://x/v1");
        assert!(t.contains("[features]"));
        assert!(
            t.contains("plugins = false"),
            "otherwise every child leaves a network fetch behind it"
        );
    }

    #[test]
    fn the_bridge_declaration_carries_its_subcommand() {
        let t = config_toml(&bridge_env(), "http://x/v1");
        assert!(t.contains(r#"args = ["mcp"]"#));
        assert!(t.contains("[mcp_servers.marion]"));
    }

    /// **The gap this closed.** `config_toml` emitted no `env` at all, so a codex node's bridge got
    /// none of the five variables it reads: a codex *root*'s `spawn` failed outright with
    /// `marion: MARION_REPO is not set`, and `TaskContract.requester` fell back to the
    /// `"unattributed-root"` placeholder — §6.7's audit record naming an agent-dir that does not
    /// exist. Each assertion below fails against the pre-fix document.
    #[test]
    fn the_nodes_identity_and_marions_own_paths_reach_the_codex_bridge() {
        let t = config_toml(&bridge_env(), "http://x/v1");
        for expected in [
            r#"MARION_REPO = "/repo""#,
            r#"MARION_STATE_DIR = "/state""#,
            r#"MARION_BASE_URL = "http://127.0.0.1:8099/v1""#,
            r#"MARION_AGENT_ID = "019f-node""#,
            // §6.1 step 2's two inputs. Without them a codex node's bridge cannot read the
            // caller's `max_depth` and every `spawn` it serves is ungated.
            r#"MARION_AGENT_TYPE = "codex-impl""#,
            r#"MARION_DEPTH = "1""#,
        ] {
            assert!(t.contains(expected), "missing {expected} from:\n{t}");
        }
        assert!(
            t.contains("env = {"),
            "the pairs must sit in an `env` table inside [mcp_servers.marion]:\n{t}"
        );
        assert!(
            !t.contains("MARION_READY_FILE"),
            "codex is LaunchOnly: there is no frame to withhold, so no marker to name"
        );
    }

    /// The marker is written when there is one, so the field is plumbed rather than merely present.
    #[test]
    fn a_readiness_marker_is_declared_when_the_surface_has_one() {
        let mut e = bridge_env();
        e.ready_file = Some("/state/x/mcp-ready".into());
        assert!(
            config_toml(&e, "http://x/v1").contains(r#"MARION_READY_FILE = "/state/x/mcp-ready""#)
        );
    }

    /// A path with a quote in it must not be able to produce a `config.toml` codex cannot parse.
    #[test]
    fn values_are_escaped_rather_than_interpolated_raw() {
        let mut e = bridge_env();
        e.repo = "/re\"po".into();
        let t = config_toml(&e, "http://x/v1");
        assert!(t.contains(r#"MARION_REPO = "/re\"po""#), "{t}");
    }

    /// **The live route carries the same declaration, not a smaller one.**
    ///
    /// Asserted key by key rather than against a rendered blob, because the failure being defended
    /// against is one key quietly going missing — and codex accepts an unknown `-c` key without a
    /// word, so a typo here is invisible at runtime. Every spelling below was checked against the
    /// installed 0.146.0 through `codex mcp get marion --json` (see [`live_config_overrides`]).
    #[test]
    fn the_live_declaration_reaches_the_bridge_through_c_flags_instead_of_a_document() {
        let o = live_config_overrides(&bridge_env());
        let by_key = |k: &str| {
            o.iter()
                .find(|(n, _)| n == k)
                .unwrap_or_else(|| panic!("missing {k} from {o:?}"))
                .1
                .clone()
        };
        assert_eq!(
            by_key("mcp_servers.marion.command"),
            r#""/bin/marion-supervisor""#
        );
        assert_eq!(by_key("mcp_servers.marion.args"), r#"["mcp"]"#);
        for (k, v) in [
            ("MARION_REPO", r#""/repo""#),
            ("MARION_STATE_DIR", r#""/state""#),
            ("MARION_BASE_URL", r#""http://127.0.0.1:8099/v1""#),
            ("MARION_AGENT_ID", r#""019f-node""#),
            // §6.1 step 2's two inputs: without them the live node's bridge cannot read the
            // caller's `max_depth` and every `spawn` it serves is ungated.
            ("MARION_AGENT_TYPE", r#""codex-impl""#),
            ("MARION_DEPTH", r#""1""#),
            ("MARION_AUTH", r#""canned""#),
        ] {
            assert_eq!(by_key(&format!("mcp_servers.marion.env.{k}")), v);
        }
    }

    /// The same trap as [`the_approval_mode_that_silently_cancels_everything_is_always_written`],
    /// on the other route. §12 records it as what would have made S6 answer its own question wrong.
    #[test]
    fn the_approval_mode_that_silently_cancels_everything_rides_the_live_route_too() {
        let o = live_config_overrides(&bridge_env());
        assert!(
            o.contains(&(
                "mcp_servers.marion.default_tools_approval_mode".to_string(),
                r#""approve""#.to_string()
            )),
            "without it every marion tool call is cancelled and the bridge never sees tools/call: \
             {o:?}"
        );
    }

    /// **marion's tools are offered directly, never deferred behind `tool_search`.** codex 0.155.1
    /// lazy-loads MCP tools by default (`codex features list`: `tool_search_always_defer_mcp_tools
    /// removed true`, so no feature flag turns it off). Measured on 2026-09-22 with a stub
    /// `marion` server: asked which tools it could call directly, a `gpt-5.6-luna` node answered
    /// that `report` from `marion` was *absent*, and with `omit_tools_from = ["deferred"]` it named
    /// `mcp__marion__report` as directly callable. `codex mcp get` on 0.147.0 and 0.155.1 rejects
    /// an unknown value of this key (`expected one of code_mode, deferred, direct`), which is the
    /// proof it is parsed; 0.146.0 accepts it as an unknown key and is unchanged.
    #[test]
    fn marions_tools_are_never_deferred_behind_tool_search_on_the_live_route() {
        assert!(
            live_config_overrides(&bridge_env()).contains(&(
                "mcp_servers.marion.omit_tools_from".to_string(),
                r#"["deferred"]"#.to_string()
            )),
            "a live codex child that has to search for `report` is a child that never calls it"
        );
    }

    /// The canned document owes the same key as the live pairs. A canned node names no model, so
    /// 0.155.1 runs it on its catalog default (`gpt-6-astra`, a code-mode model), and measured
    /// 2026-09-27 through `codex app-server` against the canned provider
    /// (`tests/fixtures/app-server-0.155.1/`, P3): without `omit_tools_from` the `exec`
    /// declaration listed none of `mcp__marion__*`, and with it every verb was listed. Canned
    /// cells stayed green only because the canned script calls `report` without reading the
    /// declaration first.
    #[test]
    fn marions_tools_are_never_deferred_behind_tool_search_on_the_canned_route() {
        let t = config_toml(&bridge_env(), "http://x/v1");
        let table = t
            .split("[mcp_servers.marion]")
            .nth(1)
            .expect("the document declares marion's server");
        assert!(
            table
                .lines()
                .take_while(|l| !l.starts_with('['))
                .any(|l| l == r#"omit_tools_from = ["deferred"]"#),
            "the key must sit inside [mcp_servers.marion], as on the live route:\n{t}"
        );
    }

    /// The runaway `git fetch` is prevented on the live route too — and it has to be prevented
    /// *here*, at config time, because after `exec` exits its descendants have reparented to pid 1
    /// and no ancestry walk can find them.
    #[test]
    fn the_background_plugin_fetch_that_outlives_exec_is_disabled_on_the_live_route_too() {
        assert!(
            live_config_overrides(&bridge_env())
                .contains(&("features.plugins".to_string(), "false".to_string())),
            "otherwise every live node leaves a network fetch behind it, writing into an agent \
             dir marion is deleting"
        );
    }

    /// The live overlay adds exactly what the canned document adds and nothing that belongs to the
    /// canned *provider*: naming `model_provider` or `MARION_PROVIDER_KEY` here would point a node
    /// holding the operator's real credential at marion's fake endpoint.
    #[test]
    fn the_live_overlay_names_no_canned_provider_and_no_minted_key() {
        for (k, v) in live_config_overrides(&bridge_env()) {
            let pair = format!("{k}={v}");
            for forbidden in ["model_provider", "MARION_PROVIDER_KEY", "base_url ="] {
                assert!(
                    !k.contains(forbidden),
                    "a live node uses codex's own default provider and the operator's own \
                     credential, but {pair} names {forbidden}"
                );
            }
        }
    }

    /// A path with a quote in it must not be able to produce a `-c` value codex parses as something
    /// else — the same escaping the document route has always had, on the route that reaches a
    /// shell-free argv but is still TOML-parsed by codex.
    #[test]
    fn live_override_values_are_escaped_rather_than_interpolated_raw() {
        let mut e = bridge_env();
        e.repo = "/re\"po".into();
        assert!(live_config_overrides(&e).contains(&(
            "mcp_servers.marion.env.MARION_REPO".to_string(),
            r#""/re\"po""#.to_string()
        )));
    }

    /// **The whole of live auth on this harness.** `CODEX_HOME` is where codex resolves
    /// `auth.json`, and S8 measured codex's to be a plain 0600 file rather than a Keychain item —
    /// so *not setting the variable* is what lets the child find the operator's own login. Absent
    /// by name, not merely different: a blank value would send codex looking in the process cwd.
    #[test]
    fn a_live_node_sets_no_codex_home_at_all_so_it_finds_the_operators_own_auth_json() {
        let inv = compile_exec(&live_spec());
        assert!(
            !inv.env.iter().any(|(k, _)| k == "CODEX_HOME"),
            "{:?}",
            inv.env
        );
        assert!(
            inv.env.is_empty(),
            "and nothing else crept in: {:?}",
            inv.env
        );
    }

    /// The `-c` pairs reach argv as `-c key=value`, one flag per pair, unquoted by marion — nothing
    /// here goes through a shell, so codex receives the TOML exactly as written.
    #[test]
    fn config_overrides_ride_argv_one_flag_per_pair() {
        let inv = compile_exec(&live_spec());
        // The row's update policy leads, then the live declaration's pairs in their own order.
        let expected: Vec<String> = SPEC
            .updates
            .pair()
            .into_iter()
            .chain(live_config_overrides(&BridgeEnv {
                auth: Auth::Inherited,
                base_url: None,
                ..bridge_env()
            }))
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        assert_eq!(
            pairs(&inv),
            expected,
            "one `-c key=value` per pair, in order"
        );
        assert_eq!(
            inv.args.iter().filter(|a| *a == "-c").count(),
            expected.len()
        );
        assert_eq!(
            inv.args.last().unwrap(),
            "do the task",
            "the prompt stays last and positional"
        );
    }

    /// **The comment this corrects said `codex exec` "takes no model argument here at all".** It
    /// does: 0.146.0's `codex exec --help` lists `-m, --model <MODEL>  Model the agent should use`.
    #[test]
    fn exec_does_take_a_model_and_records_the_one_it_compiled() {
        // Under a real vendor: a canned launch compiles none whatever was asked for.
        let inv = compile_exec(&Fields {
            model: Some("gpt-5-codex".into()),
            ..live_spec()
        });
        assert_eq!(
            inv.args.windows(2).find(|w| w[0] == "-m").map(|w| &w[1]),
            Some(&"gpt-5-codex".to_string())
        );
        assert_eq!(inv.model.as_deref(), Some("gpt-5-codex"));
    }

    /// And absent it there is still no `-m` and still nothing recorded — so a contract can never
    /// name a model that did not reach argv.
    #[test]
    fn no_model_asked_for_is_no_model_on_the_wire_and_none_recorded() {
        let inv = compile_exec(&spec());
        assert!(!inv.args.iter().any(|a| a == "-m"));
        assert_eq!(inv.model, None);
    }

    #[test]
    fn m1_leaves_output_schema_unset_because_s6_proved_the_primary_branch() {
        let inv = compile_exec(&spec());
        assert!(!inv.args.iter().any(|a| a == "--output-schema"));
    }

    #[test]
    fn the_fallback_branch_still_compiles_when_asked() {
        let inv = compile_exec(&Fields {
            output_schema: Some("/tmp/schema.json".into()),
            output_last_message: Some("/tmp/last.txt".into()),
            ..spec()
        });
        assert!(inv.args.iter().any(|a| a == "--output-schema"));
        assert!(inv.args.iter().any(|a| a == "--output-last-message"));
    }

    /// The server's side of an S36 capture: every frame app-server wrote, in order.
    fn s36(file: &str) -> Vec<Value> {
        crate::stream::json_frames(&marion_testsupport::app_server_capture(file))
    }

    /// [`s36`] as the stdout a node's driver records: one frame per line.
    fn s36_stdout(file: &str) -> String {
        marion_testsupport::app_server_capture(file)
    }

    #[test]
    fn the_app_server_vocabulary_is_well_formed_and_its_sandbox_is_the_one_marion_records() {
        crate::rpc_channel::tests::assert_well_formed(&APP);
        let open = APP.opening(1, "/wt", None, false);
        assert_eq!(open["method"], "thread/start");
        assert_eq!(open["params"]["cwd"], "/wt");
        assert_eq!(open["params"]["sandbox"], SANDBOX_MODE);
        assert_eq!(
            open["params"]["ephemeral"], false,
            "a thread marion may resume must be persisted (P8: an ephemeral one is gone)"
        );
        assert!(open.get("jsonrpc").is_none(), "P2: no jsonrpc member");
        let resume = APP.opening(1, "/wt", Some("t-9"), false);
        assert_eq!(resume["method"], "thread/resume");
        assert_eq!(resume["params"]["threadId"], "t-9");
        assert_eq!(
            resume["params"]["sandbox"], SANDBOX_MODE,
            "P8: resume takes it too"
        );
        assert_eq!(APP.opened_by(&open), Some(None));
        assert_eq!(APP.opened_by(&resume), Some(Some("t-9".into())));
        // A reviewer's thread is read-only on both requests: the request's sandbox beats argv.
        for thread in [None, Some("t-9")] {
            let ro = APP.opening(1, "/wt", thread, true);
            assert_eq!(ro["params"]["sandbox"], "read-only", "{thread:?}");
            assert_eq!(ro["params"]["ephemeral"], false, "{thread:?}");
        }
    }

    /// The turn requests carry what P6/P7 sent: the thread, the expected turn on a steer, the turn
    /// on an interrupt, and the text as one `text` item.
    #[test]
    fn the_turn_requests_are_the_shapes_s36_sent() {
        let start = APP.turn_request(3, "t", "go");
        assert_eq!(start["method"], "turn/start");
        assert_eq!(
            start["params"],
            serde_json::json!({"threadId": "t", "input": [{"type": "text", "text": "go"}]})
        );
        let steer = APP.steer_request(4, "t", "u", "more");
        assert_eq!(steer["method"], "turn/steer");
        assert_eq!(steer["params"]["expectedTurnId"], "u");
        assert_eq!(steer["params"]["input"][0]["text"], "more");
        let stop = APP.interrupt_request(5, "t", "u");
        assert_eq!(
            stop,
            serde_json::json!({"method": "turn/interrupt", "id": 5, "params": {"threadId": "t", "turnId": "u"}})
        );
    }

    /// Read off P4's capture: the thread, the turn's opening and close under one id, the
    /// command's process, and marion's server becoming ready.
    #[test]
    fn a_turns_life_reads_off_the_p4_capture() {
        let frames = s36("p4-items.jsonl");
        let thread = frames
            .iter()
            .find_map(|f| APP.thread_of(f))
            .expect("no thread/start answer is kept in p4, but thread/started is");
        let _ = thread;
        let opened: Vec<String> = frames.iter().filter_map(|f| APP.opens_turn(f)).collect();
        let closed: Vec<String> = frames.iter().filter_map(|f| APP.closes_turn(f)).collect();
        assert!(!opened.is_empty());
        assert_eq!(
            opened, closed,
            "every turn opened is closed under its own id"
        );
        let pids: Vec<i32> = frames.iter().filter_map(|f| APP.process_of(f)).collect();
        assert!(pids.contains(&23220), "{pids:?}");
        let ended: Vec<i32> = frames.iter().filter_map(|f| APP.process_ended(f)).collect();
        assert!(
            ended.contains(&23220),
            "the command's completion names it: {ended:?}"
        );
        let ready = s36("p3-mcp-readiness.jsonl");
        assert!(
            ready
                .iter()
                .any(|f| APP.startup(f, "marion") == Some(crate::rpc_channel::Startup::Ready))
        );
        assert!(
            ready.iter().all(|f| APP.startup(f, "github").is_none()),
            "another server's state is not marion's"
        );
    }

    /// A start answer names its turn, and a steer answer the turn it joined (P6).
    #[test]
    fn a_start_and_a_steer_answer_name_their_turn() {
        let start =
            serde_json::json!({"id": 3, "result": {"turn": {"id": "u-1", "status": "inProgress"}}});
        let steer = serde_json::json!({"id": 4, "result": {"turnId": "u-1"}});
        assert_eq!(APP.answered_turn(&start).as_deref(), Some("u-1"));
        assert_eq!(APP.answered_turn(&steer).as_deref(), Some("u-1"));
    }

    /// A failed server start closes the gate in the server's words (P3's missing bridge).
    #[test]
    fn a_failed_startup_closes_the_gate_in_its_own_words() {
        let failed = serde_json::json!({"method": "mcpServer/startupStatus/updated", "params": {
            "threadId": "t", "name": "marion", "status": "failed",
            "error": "MCP client for `marion` failed to start: No such file"}});
        assert_eq!(
            APP.startup(&failed, "marion"),
            Some(crate::rpc_channel::Startup::Failed(
                "MCP client for `marion` failed to start: No such file".into()
            ))
        );
    }

    /// **Approvals are declined, never cancelled** (P5: a cancel ends the turn), each in the shape
    /// its request takes, and anything else is refused by name rather than left unanswered.
    #[test]
    fn every_approval_s36_saw_is_declined_and_nothing_is_left_unanswered() {
        let asked: Vec<Value> = ["p5-approvals.jsonl", "p5-mcp-approvals.jsonl"]
            .iter()
            .flat_map(|f| s36(f))
            .filter(|f| f.get("id").is_some() && f.get("method").is_some())
            .collect();
        assert!(asked.len() >= 3, "{asked:?}");
        for request in &asked {
            let reply = APP.answer(request);
            assert_eq!(reply["id"], request["id"]);
            let text = reply.to_string();
            assert!(text.contains("decline"), "{request} -> {reply}");
            assert!(!text.contains("cancel"), "{request} -> {reply}");
        }
        let other =
            APP.answer(&serde_json::json!({"id": 9, "method": "item/tool/requestUserInput"}));
        assert_eq!(other["error"]["code"], -32601);
    }

    /// **The app-server stream reads as the exec one does**: P4's turn reports, changes a file,
    /// names its session and spends tokens, and the reading comes off the row's grammar.
    #[test]
    fn the_p4_capture_reads_through_the_app_server_grammar() {
        let stdout = s36_stdout("p4-items.jsonl");
        let out = crate::grammar::parse_stream(&APP_STREAM, &stdout, "");
        // P4 reports twice, directly and then from code-mode JS; the later report is the reading.
        assert_eq!(out.narrative.as_deref(), Some("p4 via js"));
        assert_eq!(out.failure, None);
        assert_eq!(out.file_change_paths.len(), 1, "{out:?}");
        let frames = crate::stream::json_frames(&stdout);
        let usage =
            crate::grammar::usage(APP_STREAM.usage.as_ref().unwrap(), &frames).expect("usage");
        // The thread's running total: the latest notification's `total` is the whole spend.
        let last = frames
            .iter()
            .rfind(|f| f["method"] == "thread/tokenUsage/updated")
            .expect("p4 carries usage");
        let total = &last["params"]["tokenUsage"]["total"];
        assert_eq!(
            (usage.output, usage.cache_read),
            (
                total["outputTokens"].as_u64().unwrap(),
                total["cachedInputTokens"].as_u64().unwrap()
            ),
            "{usage:?}"
        );
        let thread = serde_json::json!({"id": 2, "result": {"thread": {"id": "t-7"}}});
        assert_eq!(
            crate::grammar::session_id(&APP_STREAM, &thread).as_deref(),
            Some("t-7")
        );
    }

    /// **An item that completes after its turn is still read** (P7: the interrupted turn's command
    /// completed ~20 s after `turn/completed`).
    #[test]
    fn an_item_completed_after_its_turn_is_still_read() {
        let frames = s36("p7-interrupt.jsonl");
        let closed_at = frames
            .iter()
            .position(|f| APP.closes_turn(f).is_some())
            .expect("the interrupt closes a turn");
        assert!(
            frames[closed_at..]
                .iter()
                .any(|f| f["method"] == "item/completed"),
            "the capture holds a late item"
        );
        let items = crate::grammar::activity_stream(APP_STREAM.activity.as_ref().unwrap(), &frames);
        assert!(!items.is_empty());
    }

    #[test]
    fn app_server_carries_the_update_switch_and_the_model_as_configuration() {
        let inv = compile_app_server(&Fields {
            model: Some("gpt-5.1-codex".into()),
            ..spec()
        });
        assert_eq!(inv.args[0], "app-server");
        assert_eq!(
            pairs(&inv),
            [
                "check_for_update_on_startup=false",
                "model=\"gpt-5.1-codex\""
            ]
        );
        assert!(
            !inv.args.iter().any(|a| a == "do the task"),
            "the prompt is a turn, never argv"
        );
        let resumed = compile_app_server(&Fields {
            resume: Some("t-1".into()),
            ..spec()
        });
        assert!(
            !resumed.args.iter().any(|a| a == "t-1"),
            "a resume is thread/resume, never argv: {:?}",
            resumed.args
        );
    }
}
