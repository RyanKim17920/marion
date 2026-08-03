//! The Codex adapter — `exec` surface only, which is what M1's child uses (design §9).
//!
//! `codex exec` is **launch-only with protocol events**: marion writes its configuration, starts
//! it, and reads its JSONL stream. There is no control channel to steer it mid-turn.

use std::path::PathBuf;

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

/// The `$CODEX_HOME/config.toml` marion writes for a child.
///
/// **`default_tools_approval_mode = "approve"` is load-bearing**: without it every marion tool
/// call is silently cancelled — no error the child can act on, and the run ends `Unreported`.
pub fn config_toml(bridge: &str, bridge_args: &[&str], base_url: &str) -> String {
    let args = bridge_args
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(", ");
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
command = "{bridge}"
args = [{args}]
default_tools_approval_mode = "approve"
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

    #[test]
    fn the_approval_mode_that_silently_cancels_everything_is_always_written() {
        let t = config_toml(
            "/bin/marion-supervisor",
            &["mcp"],
            "http://127.0.0.1:8099/v1",
        );
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
        let t = config_toml("/bin/marion-supervisor", &["mcp"], "http://x/v1");
        assert!(t.contains("[features]"));
        assert!(
            t.contains("plugins = false"),
            "otherwise every child leaves a network fetch behind it"
        );
    }

    #[test]
    fn the_bridge_declaration_carries_its_subcommand() {
        let t = config_toml("/bin/marion-supervisor", &["mcp"], "http://x/v1");
        assert!(t.contains(r#"args = ["mcp"]"#));
        assert!(t.contains("[mcp_servers.marion]"));
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
