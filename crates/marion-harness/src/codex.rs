//! The Codex adapter — `exec` surface only, which is what M1's child uses (design §9).
//!
//! `codex exec` is **launch-only with protocol events**: marion writes its configuration, starts
//! it, and reads its JSONL stream. There is no control channel to steer it mid-turn.

use std::path::PathBuf;

use marion_core::contract::AgentId;

// The bridge's own env-var contract, imported rather than respelled — see [`BridgeEnv`].
use crate::adapter::Auth;
use crate::claude_code::{
    AGENT_ID_ENV, AGENT_TYPE_ENV, AUTH_ENV, BASE_URL_ENV, DEPTH_ENV, READY_FILE_ENV,
};
use crate::invocation::Invocation;
use crate::stream::{StreamOutcome, json_frames};

#[derive(Debug, Clone)]
pub struct ExecSpec {
    pub cwd: PathBuf,
    /// `$CODEX_HOME`. Unlike Claude Code's config dir, isolating this does not break auth in M1,
    /// because the child runs against the canned provider — §11 item 3 owes the real-auth check.
    pub codex_home: PathBuf,
    pub prompt: String,
    /// `--output-schema`, used only on the fallback branch. S6 proved the primary branch works,
    /// so M1 leaves this unset.
    pub output_schema: Option<PathBuf>,
    pub output_last_message: Option<PathBuf>,
}

pub fn compile_exec(spec: &ExecSpec) -> Invocation {
    let mut args: Vec<String> = vec![
        "exec".into(),
        "--json".into(),
        "--skip-git-repo-check".into(),
    ];
    if let Some(s) = &spec.output_schema {
        args.push("--output-schema".into());
        args.push(s.to_string_lossy().into_owned());
    }
    if let Some(m) = &spec.output_last_message {
        args.push("--output-last-message".into());
        args.push(m.to_string_lossy().into_owned());
    }
    args.push("-C".into());
    args.push(spec.cwd.to_string_lossy().into_owned());
    args.push(spec.prompt.clone());

    Invocation {
        program: "codex".into(),
        args,
        env: vec![(
            "CODEX_HOME".into(),
            spec.codex_home.to_string_lossy().into_owned(),
        )],
        cwd: spec.cwd.clone(),
        // **Not a gap.** `codex exec` takes no model argument here at all — the model comes from
        // `$CODEX_HOME/config.toml`'s provider selection — so there is no model on this wire to
        // record, and `ExecSpec` deliberately has no field for one. A contract naming the model a
        // caller *asked* for would be the same lie `child.harness` was sourced from the adapter to
        // stop telling.
        model: None,
    }
}

/// Parse a `codex exec --json` stream.
///
/// **Moved, not rewritten.** This is `marion-supervisor::spawn::parse_child_stream` verbatim, down
/// to the match arms and the order of the two cases; only the framing call at the top and the
/// outcome type changed. `marion-supervisor::spawn`'s equivalence test replays a corpus through a
/// preserved copy of the pre-move function and asserts the two agree, because "it looks the same"
/// is not the standard this seam's Phase 1 set for itself.
///
/// `report` arrives as an `mcp_tool_call` item whose `server` is marion — the shape S6 fixtured.
/// `file_change` items are recorded as corroborating evidence only: **git is the authority** for
/// `changed_paths`, so a child that edits without emitting one is still caught.
///
/// **Success is not decided here.** codex emits a terminal item but no verdict marion reads, so
/// `failure` is always `None` and the contract's status comes from the reported narrative and the
/// exit code, exactly as it always has. That is a statement about codex, not a default: gemini and
/// opencode both do make failure claims in-stream, and theirs are read.
pub fn parse_stream(s: &str) -> StreamOutcome {
    let mut out = StreamOutcome::default();
    for v in json_frames(s) {
        let item = &v["item"];
        match item["type"].as_str() {
            Some("mcp_tool_call") if item["server"] == "marion" && item["tool"] == "report" => {
                if let Some(n) = item["arguments"]["narrative"].as_str() {
                    out.narrative = Some(n.to_string());
                }
            }
            Some("file_change") => {
                if let Some(cs) = item["changes"].as_array() {
                    for c in cs {
                        if let Some(p) = c["path"].as_str() {
                            out.file_change_paths.push(PathBuf::from(p));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Every marion tool this stream shows the node calling, in **marion's** vocabulary.
///
/// Codex is the one harness that does **not** name marion's verbs by a prefixed identifier in its
/// stream: an `mcp_tool_call` item carries `server` and `tool` as separate fields, which is why
/// this reader takes no prefix and a substring scan for `mcp__marion__` would find nothing here.
/// That difference is exactly why the read is behind the adapter seam rather than done once in the
/// supervisor.
pub fn marion_tool_calls(s: &str) -> Vec<String> {
    json_frames(s)
        .iter()
        .filter_map(|v| {
            let item = &v["item"];
            if item["type"] != "mcp_tool_call" || item["server"] != "marion" {
                return None;
            }
            item["tool"].as_str().map(str::to_string)
        })
        .collect()
}

/// The values [`config_toml`] writes into `[mcp_servers.marion]`.
///
/// The key names in the `env` block are the **bridge's** contract, not codex's, which is why they
/// are taken from [`crate::claude_code`]'s constants rather than respelled here: the bridge reads
/// `MARION_AGENT_ID` no matter which harness started it (`marion-supervisor::main::requester`), and
/// a second spelling would put a codex node's contract back on `"unattributed-root"`.
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
    /// `None` on a `LaunchOnly` surface: the prompt rides argv, so there is no first frame to
    /// withhold and nothing to wait on (§6.1 step 8). codex is `LaunchOnly` on every path today, so
    /// this is `None` in practice; it is carried so the field cannot be forgotten if that changes.
    pub ready_file: Option<PathBuf>,
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
/// The `env` block of `[mcp_servers.marion]`, as ordered pairs.
///
/// Shared by the two routes codex's declaration can take — the generated `config.toml` under
/// [`Auth::Canned`] and the `-c mcp_servers.marion.env.…` overrides under [`Auth::Inherited`] — so
/// the two cannot carry different sets of variables. A live bridge that was handed fewer keys than a
/// canned one is exactly the failure class this codebase keeps re-finding: `spawn` answering
/// `MARION_REPO is not set` on the one path nobody tests automatically.
pub fn bridge_env_pairs(env: &BridgeEnv) -> Vec<(String, String)> {
    let mut pairs = vec![
        ("MARION_REPO".to_string(), env.repo.display().to_string()),
        (
            "MARION_STATE_DIR".to_string(),
            env.state.display().to_string(),
        ),
        (AUTH_ENV.to_string(), env.auth.as_wire().to_string()),
        (AGENT_ID_ENV.to_string(), env.agent_id.0.clone()),
        (AGENT_TYPE_ENV.to_string(), env.agent_type.clone()),
        (DEPTH_ENV.to_string(), env.depth.to_string()),
    ];
    // Present or absent, never empty ([`crate::claude_code::BASE_URL_ENV`]).
    if let Some(u) = &env.base_url {
        pairs.push((BASE_URL_ENV.to_string(), u.clone()));
    }
    if let Some(r) = &env.ready_file {
        pairs.push((READY_FILE_ENV.to_string(), r.display().to_string()));
    }
    pairs
}

pub fn config_toml(env: &BridgeEnv, base_url: &str) -> String {
    let args = env
        .args
        .iter()
        .map(|a| toml_str(a))
        .collect::<Vec<_>>()
        .join(", ");
    let env_table = bridge_env_pairs(env)
        .iter()
        .map(|(k, v)| format!("{k} = {}", toml_str(v)))
        .collect::<Vec<_>>()
        .join(", ");
    let bridge = toml_str(&env.bridge.to_string_lossy());
    format!(
        r#"model_provider = "canned"
approval_policy = "never"
sandbox_mode = "workspace-write"

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

    fn spec() -> ExecSpec {
        ExecSpec {
            cwd: "/tmp/wt".into(),
            codex_home: "/tmp/ch".into(),
            prompt: "do the task".into(),
            output_schema: None,
            output_last_message: None,
        }
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

    #[test]
    fn m1_leaves_output_schema_unset_because_s6_proved_the_primary_branch() {
        let inv = compile_exec(&spec());
        assert!(!inv.args.iter().any(|a| a == "--output-schema"));
    }

    #[test]
    fn the_fallback_branch_still_compiles_when_asked() {
        let mut s = spec();
        s.output_schema = Some("/tmp/schema.json".into());
        s.output_last_message = Some("/tmp/last.txt".into());
        let inv = compile_exec(&s);
        assert!(inv.args.iter().any(|a| a == "--output-schema"));
        assert!(inv.args.iter().any(|a| a == "--output-last-message"));
    }
}
