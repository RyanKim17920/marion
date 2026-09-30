//! **Spawning over a detached supervisor's socket, as a bridge would, and reading the result back.**
//!
//! Shared by `continuation.rs`, `descendant_gate.rs` and `depth_gate.rs`, which each used to carry a
//! copy. The test stands exactly where a bridge stands: it holds the caller's `agent_id` and the
//! token marion wrote into that node's declaration, and it states nothing else. It cannot state a
//! depth, an agent type or a child count, because [`SpawnCaller`] has nowhere to put them — which is
//! the property those files rest on.

use std::path::Path;

use marion_core::contract::{AgentId, TaskId};
use marion_core::proto::params::AgentSpawnParams;
use marion_core::proto::{Call, Method, MethodResult, SpawnCaller};
use marion_testsupport::{PersistedContract, persisted_contracts};
use serde_json::Value;

use super::Supervisor;

/// A node the supervisor owns, and the capability its own bridge would present.
pub struct Owned {
    pub agent_id: AgentId,
    pub task_id: Option<TaskId>,
    pub token: String,
}

/// One `agent/spawn`, answered — as a root when `caller` is `None`, as that node's child otherwise.
///
/// `Err` carries a refusal (or an undecodable result) for the tests that assert on one; the token
/// is read back off the declaration marion wrote rather than taken from the answer, so a bridge
/// started from it fails if marion ever stops writing it. `native_launch` is cleared whatever the
/// caller passed: these beds exercise the socket lane, never the native facade.
pub fn try_spawn_over_socket(
    sup: &Supervisor,
    state: &Path,
    caller: Option<&Owned>,
    p: AgentSpawnParams,
) -> Result<Owned, String> {
    let p = AgentSpawnParams {
        native_launch: None,
        caller: caller.map(|c| SpawnCaller {
            agent_id: c.agent_id.clone(),
            node_token: c.token.clone().into(),
        }),
        ..p
    };
    let answered = sup.call(Call::AgentSpawn(p))?;
    let MethodResult::AgentSpawn(r) = Method::AgentSpawn
        .decode_result(&answered)
        .map_err(|e| e.to_string())?
    else {
        panic!("agent/spawn answers with an agent/spawn result");
    };
    let token = super::declaration_of(state, &r.agent_id)
        .remove("MARION_NODE_TOKEN")
        .expect("declaration_of asserts the token is there");
    Ok(Owned {
        agent_id: r.agent_id,
        task_id: r.task_id,
        token,
    })
}

/// [`try_spawn_over_socket`] for a spawn that must be served.
pub fn spawn_over_socket(
    sup: &Supervisor,
    state: &Path,
    caller: Option<&Owned>,
    p: AgentSpawnParams,
) -> Owned {
    try_spawn_over_socket(sup, state, caller, p)
        .unwrap_or_else(|e| panic!("agent/spawn must be served: {e}"))
}

/// The spawn these beds ask for: `agent_type` on `prompt`, writing under `src/**`, bounded by
/// `timeout_secs`. A root (one given a `repo`) declines §9's change record — nothing here asserts
/// on it, and taking it would walk the fixture repo on every run.
pub fn params(
    agent_type: &str,
    prompt: String,
    repo: Option<&Path>,
    timeout_secs: u64,
) -> AgentSpawnParams {
    AgentSpawnParams {
        // A root in these beds is the operator's, opted in: they test delegation, and a codex root
        // delegating to opencode is exactly that. `containment.rs` tests the gate itself.
        uncontained_children: repo.map(|_| true),
        review_of: None,
        race: None,
        candidates: vec![],
        agent_type: agent_type.into(),
        prompt,
        native_launch: None,
        caller: None,
        repo: repo.map(Path::to_path_buf),
        acceptance_criteria: vec![],
        verification: vec![],
        writable_scope: vec![],
        timeout_secs: Some(timeout_secs),
        model: None,
        no_change_record: repo.map(|_| true),
        pane: None,
        isolation: None,
        allow_concurrent_writes: None,
        notify_parent: false,
        profile: None,
    }
}

/// The one contract marion persisted for `id`, parsed. More than one fails naming every path.
pub fn contract_of(state: &Path, id: &AgentId) -> Value {
    let all = persisted_contracts(state).expect("the state tree walks");
    let mine: Vec<&PersistedContract> = all
        .iter()
        .filter(|c| c.path.to_string_lossy().contains(&id.0))
        .collect();
    assert_eq!(
        mine.len(),
        1,
        "one contract for the child under test; found {:?}",
        mine.iter().map(|c| &c.path).collect::<Vec<_>>()
    );
    mine[0]
        .parsed
        .clone()
        .unwrap_or_else(|e| panic!("the child's contract does not read back: {e}"))
}
