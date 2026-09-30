//! The bridge's contract: the variables `marion-supervisor mcp` reads, and the one value every
//! harness's declaration of it is written from.

use std::path::PathBuf;

use marion_core::contract::AgentId;
use serde_json::{Value, json};

use crate::auth::Auth;

pub const REPO_ENV: &str = "MARION_REPO";
pub const STATE_DIR_ENV: &str = "MARION_STATE_DIR";
pub const AGENT_ID_ENV: &str = "MARION_AGENT_ID";
pub const AGENT_TYPE_ENV: &str = "MARION_AGENT_TYPE";
pub const AUTH_ENV: &str = "MARION_AUTH";
pub const BASE_URL_ENV: &str = "MARION_BASE_URL";
pub const DEPTH_ENV: &str = "MARION_DEPTH";
pub const NODE_TOKEN_ENV: &str = "MARION_NODE_TOKEN";
/// Where a harness's environment points its bridge at the node token, in place of the token
/// itself, on a row whose carrier withholds it from the declaration: the path of a 0600 file in
/// the node's own directory ([`NODE_TOKEN_FILE`]). A model's shell commands inherit the harness's
/// environment, so the token is never in it.
pub const NODE_TOKEN_FILE_ENV: &str = "MARION_NODE_TOKEN_FILE";
/// The node token's file name, under the node's own config or document directory.
pub const NODE_TOKEN_FILE: &str = "node-token";
pub const READY_FILE_ENV: &str = "MARION_READY_FILE";

/// What every MCP declaration of marion's bridge carries — **one value, five documents**.
///
/// The bridge is a *short-lived process the harness starts*, not one marion spawns (§5.4), so
/// everything it needs rides the declaration: which repo, which state dir, which provider, and
/// **which node it is serving**. `MARION_AGENT_ID` is what makes `TaskContract.requester` the
/// node's own `AgentId` rather than a placeholder; `MARION_AGENT_TYPE` and `MARION_DEPTH` are
/// §6.1 step 2's two inputs, without which a node's bridge cannot read the caller's `max_depth`
/// and every `spawn` it serves is ungated.
///
/// It used to be four structs — one per harness module, field for field the same — and the key
/// names in each were the bridge's contract respelled. A live bridge handed fewer keys than a
/// canned one is exactly the failure class this codebase keeps re-finding (`spawn` answering
/// `MARION_REPO is not set` on the one path nobody tests automatically), so the derivation is
/// [`Self::pairs`], once, and every document shape is a serialisation of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeEnv {
    pub bridge: PathBuf,
    pub args: Vec<String>,
    pub repo: PathBuf,
    pub state: PathBuf,
    /// `None` under [`Auth::Inherited`]: the key is omitted, never written empty
    /// ([`BASE_URL_ENV`]).
    pub base_url: Option<String>,
    /// Which endpoint a child this node spawns should talk to ([`AUTH_ENV`]). Stated on **every**
    /// declaration, in both modes: a key that appears only under `--live` would make its absence
    /// mean two things at once.
    pub auth: Auth,
    pub agent_id: AgentId,
    /// The node's agent type name, in its **canonical** spelling — the alias `codex` resolves to
    /// `codex-impl` before it gets here, so the bridge re-resolves one definition and not two.
    pub agent_type: String,
    /// The node's depth, root = 0 (§3.1's `max_depth`).
    pub depth: u32,
    /// §5.4's capability token for this node — `None` where the supervisor minted none. Present or
    /// absent, never empty ([`NODE_TOKEN_ENV`]).
    pub node_token: Option<marion_core::secret::Secret>,
    /// Where the node token's 0600 file is, for a declaration that names the file rather than
    /// carrying the token ([`NODE_TOKEN_FILE_ENV`]) — a declaration that itself rides the
    /// harness's environment, which every shell command its model runs inherits. `None` otherwise.
    pub node_token_file: Option<PathBuf>,
    /// The readiness marker the bridge touches once it has answered `tools/list` (§6.1 step 8).
    /// `None` on a `LaunchOnly` surface: the prompt rides argv, so there is no first frame to
    /// withhold and nothing to wait on.
    pub ready_file: Option<PathBuf>,
}

impl BridgeEnv {
    /// The `env` block of the declaration, as ordered pairs. Every document shape is written from
    /// this, so no two harnesses can hand the bridge different sets of variables.
    pub fn pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            (
                REPO_ENV.to_string(),
                self.repo.to_string_lossy().into_owned(),
            ),
            (
                STATE_DIR_ENV.to_string(),
                self.state.to_string_lossy().into_owned(),
            ),
            (AUTH_ENV.to_string(), self.auth.as_wire().to_string()),
            (AGENT_ID_ENV.to_string(), self.agent_id.0.clone()),
            (AGENT_TYPE_ENV.to_string(), self.agent_type.clone()),
            // A string, because an MCP `env` block is `Record<string,string>` on every harness
            // that has one. The bridge parses it back.
            (DEPTH_ENV.to_string(), self.depth.to_string()),
        ];
        // Present or absent, never empty ([`BASE_URL_ENV`]); the same for the other two.
        if let Some(u) = &self.base_url {
            pairs.push((BASE_URL_ENV.to_string(), u.clone()));
        }
        if let Some(t) = &self.node_token {
            pairs.push((NODE_TOKEN_ENV.to_string(), t.expose().to_string()));
        }
        if let Some(f) = &self.node_token_file {
            pairs.push((
                NODE_TOKEN_FILE_ENV.to_string(),
                f.to_string_lossy().into_owned(),
            ));
        }
        if let Some(r) = &self.ready_file {
            pairs.push((READY_FILE_ENV.to_string(), r.to_string_lossy().into_owned()));
        }
        pairs
    }

    /// [`Self::pairs`] as the JSON object the four JSON-shaped declarations carry.
    pub fn env_json(&self) -> Value {
        Value::Object(
            self.pairs()
                .into_iter()
                .map(|(k, v)| (k, json!(v)))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge(node_token: Option<&str>) -> BridgeEnv {
        BridgeEnv {
            node_token_file: None,
            bridge: "/bin/marion-supervisor".into(),
            args: vec!["mcp".into()],
            repo: "/repo".into(),
            state: "/state".into(),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
            agent_id: AgentId("019f-child".into()),
            agent_type: "codex-impl".into(),
            depth: 1,
            node_token: node_token.map(Into::into),
            ready_file: None,
        }
    }

    /// **The token is carried, but never printed.** The declaration's `env` block is where the
    /// bridge reads its capability from, so the value must reach [`BridgeEnv::pairs`] verbatim;
    /// a `{:?}` of the same struct must not show it.
    #[test]
    fn the_node_token_reaches_the_env_block_but_never_a_debug_print() {
        let b = bridge(Some("tok-SENTINEL-7c1f"));
        let printed = format!("{b:?} {b:#?}");
        assert!(!printed.contains("SENTINEL"), "{printed}");
        assert!(
            b.pairs()
                .contains(&(NODE_TOKEN_ENV.to_string(), "tok-SENTINEL-7c1f".to_string())),
            "{:?}",
            b.pairs().iter().map(|(k, _)| k).collect::<Vec<_>>()
        );
        assert_eq!(b.env_json()[NODE_TOKEN_ENV], "tok-SENTINEL-7c1f");
        assert!(
            bridge(None)
                .pairs()
                .iter()
                .all(|(k, _)| k != NODE_TOKEN_ENV)
        );
    }
}
