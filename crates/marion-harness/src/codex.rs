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

use marion_core::agent_type;
use marion_core::harness::Harness;

use crate::grammar::{
    Cond, Name, OnRefusedReport, Pairing, PathList, SessionId, StreamGrammar, Verdict, Where,
};
pub use crate::mcp_bridge::BridgeEnv;
use crate::spec::{
    Arg, Constraint, Env, Field, HarnessSpec, LiveDeclaration, McpRoute, McpRoutes, Resume,
    Spelling, Surfaces, ToolSpelling, UpdatePolicy, Val, When,
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
/// **`-s/--sandbox` and `-a/--ask-for-approval`.** Both are already set, by the same
/// [`config_toml`] the headless shape is configured with (`sandbox_mode`, `approval_policy`), and
/// a second spelling on argv is the drift [`SANDBOX_MODE`] exists to prevent.
pub const SPEC: HarnessSpec = HarnessSpec {
    harness: Harness::Codex,
    // `LaunchOnly` + `ProtocolEvents` + no display — §3.4's combination outside the four presets.
    surfaces: Surfaces::LaunchOnly,
    program: Some("codex"),
    argv: &[
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
    env: &[Env {
        key: "CODEX_HOME",
        val: Val::Under(""),
        when: When::Canned,
    }],
    stream: Some(&STREAM),
    // `write` → `sandbox:workspace-write`, §3.1's *"coarsest equivalent"* named for this exact
    // harness: `codex exec` has no `--tools` and no permission list, only `--sandbox`, and
    // [`config_toml`] compiles `workspace-write` on every node — so a declaration is **satisfied
    // rather than newly granted** and argv carries nothing for it. Making the mode conditional
    // would silently demote every codex node marion spawns today to `read-only`.
    //
    // `read` is refused, and the absence is the decision: s14 measured codex's whole declaration
    // (`apply_patch, create_goal, exec_command, …`) identical under both sandbox modes with no
    // read tool in it — reading a file is `exec_command`, the shell, which also writes and reaches
    // the network. A reader of `tools: [read]` would take a codex node for read-only when it is
    // nothing of the kind.
    tool_names: &[(agent_type::TOOL_WRITE, "sandbox:workspace-write")],
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
    // §3.1's worked example, verbatim: it replaces a hardcoded `["apply_patch", "shell"]` that
    // named tools codex never checked a call against. Constant, because [`config_toml`] compiles
    // that one sandbox mode on every node.
    constraint: Constraint::Fixed {
        prefix: "sandbox:",
        value: SANDBOX_MODE,
    },
    // The TUI's `codex resume` is a different grammar and unmeasured, so the pane row carries no
    // `Arg::Resume` and a paned resume is refused.
    resume: Some(Resume::Subcommand("resume")),
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
    note: "S6 on codex 0.146.0 for exec --json (tests/fixtures/s6); the TUI row and its \
           omissions measured on 0.147.0 for M3 C2; harness_matrix's codex cell and M1's hop run \
           the exec row end to end",
};

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
/// **No failure claim of its own**: codex emits a terminal item but no verdict marion reads, so
/// the contract's status comes from the reported narrative and the exit code, as it always has.
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
    failures: &[],
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
    session: Some(SessionId {
        at: Where {
            frame: &[Cond::Eq("/type", "thread.started")],
            each: None,
            unit: &[],
        },
        path: "/thread_id",
    }),
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
pub const SANDBOX_MODE: &str = "workspace-write";

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
/// and the canned one cannot hand the bridge different sets of variables.
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
    ];
    for (k, v) in env.pairs() {
        out.push((format!("{MCP_SERVER_KEY}.env.{k}"), toml_str(&v)));
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
sandbox_mode = "{SANDBOX_MODE}"

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
env_key = "MARION_DUMMY_KEY"

[mcp_servers.marion]
command = {bridge}
args = [{args}]
default_tools_approval_mode = "approve"
env = {{ {env_table} }}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use marion_core::contract::AgentId;

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
    /// canned *provider*: naming `model_provider` or `MARION_DUMMY_KEY` here would point a node
    /// holding the operator's real credential at marion's fake endpoint.
    #[test]
    fn the_live_overlay_names_no_canned_provider_and_no_minted_key() {
        for (k, v) in live_config_overrides(&bridge_env()) {
            let pair = format!("{k}={v}");
            for forbidden in ["model_provider", "MARION_DUMMY_KEY", "base_url ="] {
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
}
