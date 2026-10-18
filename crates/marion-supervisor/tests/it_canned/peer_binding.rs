//! **§5.4's peer binding, end to end: a node's token presented from another node's process tree is
//! named as a lateral attempt, and the same token from its own tree is not.**
//!
//! A node token is a file a same-uid sibling can read on Linux, so the token alone cannot say who
//! is speaking. The supervisor also walks the connecting process's ppid chain to the first live node
//! root (`peer_tree`). This file drives that check through every real piece: a detached supervisor,
//! two running roots, and marion's own bridge binary dialing it from inside each root's tree.
//!
//! # The bed
//!
//! `node_kill.rs`'s: a detached supervisor with the app-server `codex` shim on its `PATH`. Each
//! root's turn waits for a script this test writes, runs it, and holds on a gate. Both scripts run
//! `marion-supervisor mcp` — the bridge a real harness starts — as **node A**, with A's declared
//! environment and A's token, and call `status`:
//!
//! - from A's own tree, the control: the hello is A's, and nothing is recorded;
//! - from B's tree, the lateral attempt: the token is right and the process is B's, so the
//!   supervisor journals a `ForeignPeer` for A naming the bridge's pid and B's tree.
//!
//! The check ships recording rather than refusing (`PEER_BINDING_ENFORCED`), so both calls are
//! answered; the handler tests flip the switch and pin the refusal. With the check removed from the
//! node hello, the journal carries no `ForeignPeer` and the lateral assertion fails.
//!
//! ```sh
//! cargo test -p marion-supervisor --test it_canned peer_binding::
//! ```
//!
//! It needs no harness binary, no network and no credential.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_core::journal::ForeignPeer;
use marion_core::paths::ProjectDir;
use marion_core::proto::params::AgentSpawnParams;
use marion_core::proto::{Call, Method, MethodResult};
use marion_supervisor::socket::project_root;
use marion_testsupport::{Scratch, fixture_repo, scratch, survivors, sweep};
use serde_json::json;

use crate::common;
use common::Supervisor;

/// A bound that exists only to fail: every wait here is on work the test has already caused.
const BOUND: Duration = Duration::from_secs(60);

/// The shim's own life cap, so no bad run strands one. Far longer than any passing test.
const SHIM_LIFE: Duration = Duration::from_secs(90);

const ROOT_A: &str = "PEER-BINDING-A-41d9";
const ROOT_B: &str = "PEER-BINDING-B-41d9";

/// A `codex` whose turn runs `scripts/<marker>`, writes what it printed to `out/<marker>`, and
/// then holds until `gate` exists — so both roots are live while either bridge dials.
fn shim(dir: &Path, gate: &Path, scripts: &Path, out: &Path) {
    let ticks = SHIM_LIFE.as_millis() / 50;
    let script = format!(
        r#"#!/bin/sh
case "$1" in
  --version) echo "codex-cli 0.146.0-marion-shim"; exit 0 ;;
esac
name=other
case "$*" in
  *{a}*) name={a} ;;
  *{b}*) name={b} ;;
esac
waited=0
while [ ! -e {scripts}/$name ] && [ "$waited" -le {ticks} ]; do sleep 0.05; waited=$((waited + 1)); done
mkdir -p {out}
/bin/sh {scripts}/$name > {out}/$name.tmp 2>&1
mv {out}/$name.tmp {out}/$name
waited=0
while [ ! -e {gate} ] && [ "$waited" -le {ticks} ]; do sleep 0.05; waited=$((waited + 1)); done
exit 0
"#,
        a = ROOT_A,
        b = ROOT_B,
        gate = common::shell_quote(gate),
        scripts = common::shell_quote(scripts),
        out = common::shell_quote(out),
    );
    assert_eq!(
        common::app_server::fake_codex(dir, &script),
        dir.join("codex")
    );
}

struct Bed {
    _scratch: Scratch,
    repo: PathBuf,
    state: PathBuf,
    project: ProjectDir,
    gate: PathBuf,
    scripts: PathBuf,
    out: PathBuf,
    needle: String,
    supervisor: Supervisor,
}

impl Bed {
    fn new(tag: &str) -> Self {
        let s = scratch(tag);
        let dir = s.to_path_buf();
        let repo = fixture_repo(&dir);
        let state = dir.join("state");
        let bin = dir.join("bin");
        let gate = dir.join("gate");
        let scripts = dir.join("scripts");
        let out = dir.join("out");
        for d in [&state, &bin, &scripts] {
            std::fs::create_dir_all(d).unwrap();
        }
        shim(&bin, &gate, &scripts, &out);
        // The whole search path is stated, so no real `codex` can be what runs.
        for sys in ["/usr/bin", "/bin"] {
            assert!(
                !Path::new(sys).join("codex").exists(),
                "{sys} holds a `codex`, so this bed could launch a real harness"
            );
        }
        let key = project_root(&repo);
        // Unsandboxed: the shim reads its scripts and writes its output beside the bed.
        let supervisor = Supervisor::start_unsandboxed(
            &state,
            &key,
            &format!("{}:/usr/bin:/bin", bin.to_string_lossy()),
            // Nothing reaches it: every harness invocation is the shim.
            "http://127.0.0.1:8099/v1",
            Duration::from_secs(600),
        );
        Self {
            needle: dir.to_string_lossy().to_string(),
            project: ProjectDir::new(&state, &key),
            _scratch: s,
            repo,
            state,
            gate,
            scripts,
            out,
            supervisor,
        }
    }

    fn spawn_root(&self, marker: &str) -> AgentId {
        let answered = self
            .supervisor
            .call(Call::AgentSpawn(AgentSpawnParams {
                wider_children: None,
                budget_tokens: None,
                review_of: None,
                notify_parent: false,
                agent_type: "codex".into(),
                prompt: format!("{marker}: run the test's script, then hold until the gate opens"),
                native_launch: None,
                caller: None,
                repo: Some(self.repo.clone()),
                acceptance_criteria: vec![],
                verification: vec![],
                writable_scope: vec![],
                timeout_secs: Some(SHIM_LIFE.as_secs() + 30),
                model: None,
                no_change_record: Some(true),
                pane: None,
                isolation: None,
                allow_concurrent_writes: None,
                profile: None,
                candidates: vec![],
                race: None,
            }))
            .unwrap_or_else(|e| panic!("agent/spawn must be served: {e}"));
        let MethodResult::AgentSpawn(r) = Method::AgentSpawn.decode_result(&answered).unwrap()
        else {
            panic!("agent/spawn answers with an agent/spawn result");
        };
        r.agent_id
    }

    /// Have the root whose prompt carries `marker` run marion's bridge **as `as_node`**, with that
    /// node's declared environment and token, and call `status` once. The bridge `exec`s from the
    /// script's shell, so the pid the script records is the pid that dials the supervisor.
    ///
    /// The environment is cleared first: what the bridge knows is exactly the declaration, not
    /// whatever the hosting root's own environment carries.
    fn run_bridge_in(&self, marker: &str, as_node: &AgentId) {
        let mut declared: BTreeMap<String, String> = common::declaration_of(&self.state, as_node);
        // The token itself, not the file it rides in: what a sibling that read the file holds.
        declared.remove(marion_harness::mcp_bridge::NODE_TOKEN_FILE_ENV);
        // The readiness marker belongs to the node's own bridge; this one must not touch it.
        declared.remove(marion_harness::mcp_bridge::READY_FILE_ENV);
        assert!(
            declared.contains_key(marion_harness::mcp_bridge::NODE_TOKEN_ENV),
            "the declaration carries {}'s token: {declared:?}",
            as_node.0
        );
        let requests = self.scripts.join(format!("{marker}.jsonl"));
        let frames = [
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                   "params": {"protocolVersion": "2025-06-18"}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                   "params": {"name": "status", "arguments": {}}}),
        ];
        let text: String = frames.iter().map(|f| format!("{f}\n")).collect();
        std::fs::write(&requests, text).unwrap();
        let assignments: String = declared
            .iter()
            .map(|(k, v)| format!(" {k}={}", common::shell_quote(Path::new(v))))
            .collect();
        let script = format!(
            "echo $$ > {pid}\nexec env -i PATH=/usr/bin:/bin{assignments} {bridge} mcp < {requests}\n",
            pid = common::shell_quote(&self.out.join(format!("{marker}.pid"))),
            bridge = common::shell_quote(Path::new(env!("CARGO_BIN_EXE_marion-supervisor"))),
            requests = common::shell_quote(&requests),
        );
        // Written whole, then renamed: the shim runs the first file it sees at this name.
        let tmp = self.scripts.join(format!("{marker}.tmp"));
        std::fs::write(&tmp, script).unwrap();
        std::fs::rename(&tmp, self.scripts.join(marker)).unwrap();
    }

    /// What the bridge run in `marker`'s root printed, and the pid it dialed from.
    fn bridge_answer(&self, marker: &str) -> (String, u32) {
        let printed = wait_until("the root's script to finish", || {
            std::fs::read_to_string(self.out.join(marker)).ok()
        });
        let pid = std::fs::read_to_string(self.out.join(format!("{marker}.pid")))
            .expect("the script recorded its pid")
            .trim()
            .parse()
            .expect("a pid");
        (printed, pid)
    }

    fn foreign_peers(&self) -> Vec<ForeignPeer> {
        common::journal::foreign_peers(&self.project.journal())
    }
}

impl Drop for Bed {
    /// Release every shim, end the supervisor, then prove nothing is left.
    fn drop(&mut self) {
        let _ = std::fs::write(&self.gate, b"go");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !survivors(&self.needle).is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.supervisor.stop();
        let left = sweep(&self.needle);
        if !std::thread::panicking() {
            assert!(left.is_empty(), "the bed left live processes: {left:?}");
        }
    }
}

fn wait_until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + BOUND;
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(Instant::now() < deadline, "waited {BOUND:?} for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The bridge's answer to the `status` call (id 1), which must be a tool result and not an error.
fn assert_status_answered(printed: &str, whose: &str) {
    let answered = printed
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|f| f["id"] == 1)
        .unwrap_or_else(|| panic!("{whose}: the bridge never answered `status`:\n{printed}"));
    assert!(
        answered.get("result").is_some() && answered["result"]["isError"] != true,
        "{whose}: `status` was not served: {answered}"
    );
}

/// Node B's tree presents node A's token: recorded as a lateral attempt naming the bridge and B.
/// A's own tree presents the same token: nothing recorded.
#[test]
fn a_nodes_token_from_a_sibling_tree_is_journaled_and_from_its_own_tree_is_not() {
    let bed = Bed::new("peer-binding");
    let a = bed.spawn_root(ROOT_A);
    let b = bed.spawn_root(ROOT_B);

    // The control first, and alone, so a record it caused could not hide behind the lateral one.
    bed.run_bridge_in(ROOT_A, &a);
    let (printed, own_pid) = bed.bridge_answer(ROOT_A);
    assert_status_answered(&printed, "A's own bridge");
    assert_eq!(
        bed.foreign_peers(),
        vec![],
        "a bridge inside A's own tree is A's (pid {own_pid}); nothing is lateral about it"
    );

    bed.run_bridge_in(ROOT_B, &a);
    let (printed, lateral_pid) = bed.bridge_answer(ROOT_B);
    // Recorded, not refused, while the check only records.
    assert_status_answered(&printed, "A's token from B's tree");
    let records = bed.foreign_peers();
    assert!(
        !records.is_empty(),
        "A's token was presented from B's tree (pid {lateral_pid}) and no ForeignPeer was journaled"
    );
    for r in &records {
        assert_eq!(
            r.agent_id, a,
            "the record names the node whose token it was: {r:?}"
        );
        assert_eq!(
            r.peer_pid,
            Some(lateral_pid),
            "and the process that presented it: {r:?}"
        );
        assert!(
            r.why.contains(&format!("node `{}`'s process tree", b.0)),
            "and the tree it came from: {r:?}"
        );
        assert!(!r.refused, "the check only records for now: {r:?}");
    }
}
