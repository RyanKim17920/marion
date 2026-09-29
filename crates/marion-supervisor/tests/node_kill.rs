//! **§2's `node/kill`, end to end: a real running node killed over the socket ends
//! `Exited(Cancelled)`, its whole process tree is dead, and it is still `Cancelled` after the
//! node's own thread has finished.**
//!
//! The last clause is the one a handler-level test cannot reach. A node the supervisor owns has a
//! thread that is blocked on its process for the node's whole life; when the kill lands, that thread
//! wakes to a SIGKILLed process and goes on to write the node's end the way it always does. If it
//! wrote an `Exited` of its own after the kill's `KillConfirmed`, replay would refold the node to
//! `Failed` — a deliberate cancellation rewritten as a failure, one record later. So every test here
//! waits for **the thread's own last write** (the stream's closing bookend for a root, the
//! `ContractPersisted` journal record for a child) and only then reads the journal.
//!
//! # The bed
//!
//! A detached supervisor with a `codex` shim on its `PATH`, the way `background_spawn.rs` builds
//! one. Every node is the shim: a real process marion really launches, blocked on a gate file this
//! test holds shut, with one **descendant** of its own (a subshell in its process group) so the
//! assertion is about the node's process *tree* and not just its leader. Each invocation writes
//! `<leader pid> <descendant pid>` to a file named for the marker in its prompt, which is how the
//! test knows which pids belong to which node without asking marion.
//!
//! ```sh
//! cargo test -p marion-supervisor --test node_kill
//! ```
//!
//! It needs no harness binary, no network and no credential.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marion_core::contract::{AgentId, ExitStatus, TaskContract};
use marion_core::event::{Lifecycle, Payload};
use marion_core::journal::RecordKind;
use marion_core::node::NodeState;
use marion_core::paths::ProjectDir;
use marion_core::proto::params::{AgentSpawnParams, NodeGetParams, NodeKillParams};
use marion_core::proto::{Call, Method, MethodResult, SpawnCaller};
use marion_supervisor::events::EventReader;
use marion_supervisor::socket::project_root;
use marion_testsupport::{
    Liveness, Scratch, fixture_repo, liveness, persisted_contracts, scratch, survivors, sweep,
};

mod common;
use common::Supervisor;

/// A bound that exists only to fail: every wait here is on work the test has already caused.
const BOUND: Duration = Duration::from_secs(60);

/// The shim's own life cap, so no bad run strands one. Far longer than any passing test.
const SHIM_LIFE: Duration = Duration::from_secs(90);

const VICTIM_ROOT: &str = "NODE-KILL-VICTIM-ROOT-7c2e";
const SURVIVOR_ROOT: &str = "NODE-KILL-SURVIVOR-ROOT-7c2e";
const VICTIM_CHILD: &str = "NODE-KILL-VICTIM-CHILD-7c2e";

/// A `codex` that blocks on `gate`, with one descendant in its own process group, and records both
/// pids under `pids/<marker>` — the app-server shim (`common::app_server`), whose turn runs this
/// hook with the prompt as `$1`; the leader is the server marion spawned, the hook's parent.
fn shim(dir: &Path, gate: &Path, pids: &Path) {
    let bin = dir.join("codex");
    let ticks = SHIM_LIFE.as_millis() / 50;
    let script = format!(
        r#"#!/bin/sh
case "$1" in
  --version) echo "codex-cli 0.146.0-marion-shim"; exit 0 ;;
esac
name=other
case "$*" in
  *{victim_root}*) name={victim_root} ;;
  *{survivor_root}*) name={survivor_root} ;;
  *{victim_child}*) name={victim_child} ;;
esac
# The descendant: a subshell in this process's group, alive exactly as long as the leader waits.
( waited=0
  while [ ! -e {gate} ] && [ "$waited" -le {ticks} ]; do sleep 0.05; waited=$((waited + 1)); done ) &
mkdir -p {pids}
echo "$PPID $!" > {pids}/$name.tmp && mv {pids}/$name.tmp {pids}/$name
waited=0
while [ ! -e {gate} ] && [ "$waited" -le {ticks} ]; do
  sleep 0.05
  waited=$((waited + 1))
done
exit 0
"#,
        victim_root = VICTIM_ROOT,
        survivor_root = SURVIVOR_ROOT,
        victim_child = VICTIM_CHILD,
        gate = common::shell_quote(gate),
        pids = common::shell_quote(pids),
    );
    assert_eq!(common::app_server::fake_codex(dir, &script), bin);
}

struct Bed {
    _scratch: Scratch,
    repo: PathBuf,
    state: PathBuf,
    project: ProjectDir,
    gate: PathBuf,
    pids: PathBuf,
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
        let pids = dir.join("pids");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        shim(&bin, &gate, &pids);
        // The whole search path is stated, so no real `codex` can be what runs.
        for sys in ["/usr/bin", "/bin"] {
            assert!(
                !Path::new(sys).join("codex").exists(),
                "{sys} holds a `codex`, so this bed could launch a real harness"
            );
        }
        let key = project_root(&repo);
        let supervisor = Supervisor::start(
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
            pids,
            supervisor,
        }
    }

    fn spawn(&self, p: AgentSpawnParams) -> AgentId {
        let answered = self
            .supervisor
            .call(Call::AgentSpawn(p))
            .unwrap_or_else(|e| panic!("agent/spawn must be served: {e}"));
        let MethodResult::AgentSpawn(r) = Method::AgentSpawn.decode_result(&answered).unwrap()
        else {
            panic!("agent/spawn answers with an agent/spawn result");
        };
        r.agent_id
    }

    fn spawn_root(&self, marker: &str) -> AgentId {
        self.spawn(AgentSpawnParams {
            wider_children: None,
            budget_tokens: None,
            review_of: None,
            notify_parent: false,
            agent_type: "codex".into(),
            prompt: format!("{marker}: hold until the gate opens"),
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
        })
    }

    fn spawn_child(&self, parent: &AgentId, marker: &str) -> AgentId {
        let token = common::declaration_of(&self.state, parent)
            .remove("MARION_NODE_TOKEN")
            .expect("declaration_of asserts the token is there");
        self.spawn(AgentSpawnParams {
            wider_children: None,
            budget_tokens: None,
            review_of: None,
            notify_parent: false,
            agent_type: "codex-impl".into(),
            prompt: format!("{marker}: hold until the gate opens"),
            native_launch: None,
            caller: Some(SpawnCaller {
                agent_id: parent.clone(),
                node_token: token.into(),
            }),
            repo: None,
            acceptance_criteria: vec![],
            // A command that would leave a trace if it ran: a killed node's workspace is whatever
            // the kill left, and nothing is run over it.
            verification: vec!["true".into()],
            writable_scope: vec!["src/**".into()],
            timeout_secs: Some(SHIM_LIFE.as_secs() + 30),
            model: None,
            no_change_record: None,
            pane: None,
            isolation: None,
            allow_concurrent_writes: None,
            profile: None,
            candidates: vec![],
            race: None,
        })
    }

    fn kill(&self, id: &AgentId) -> Result<NodeState, String> {
        let answered = self.supervisor.call(Call::NodeKill(NodeKillParams {
            agent_id: id.clone(),
        }))?;
        let MethodResult::NodeKill(r) = Method::NodeKill.decode_result(&answered).unwrap() else {
            panic!("node/kill answers with a node/kill result");
        };
        Ok(r.state)
    }

    fn state_over_socket(&self, id: &AgentId) -> NodeState {
        let answered = self
            .supervisor
            .call(Call::NodeGet(NodeGetParams::of(id.clone())))
            .expect("node/get is served");
        let MethodResult::NodeGet(r) = Method::NodeGet.decode_result(&answered).unwrap() else {
            panic!("node/get answers with a node/get result");
        };
        r.node.state
    }

    /// `(leader, descendant)` for the node whose prompt carried `marker`, once the shim wrote them.
    fn pids_of(&self, marker: &str) -> (i32, i32) {
        let path = self.pids.join(marker);
        let text = wait_until("the shim to record its pids", || {
            std::fs::read_to_string(&path).ok()
        });
        let mut it = text.split_whitespace().map(|p| p.parse::<i32>().unwrap());
        (it.next().unwrap(), it.next().unwrap())
    }

    /// Every record the journal holds about `id`, in order.
    fn records_about(&self, id: &AgentId) -> Vec<RecordKind> {
        common::journal::records(&self.project.journal())
            .into_iter()
            .filter(|k| record_agent(k) == Some(id))
            .collect()
    }

    /// The stream's closing bookend for `id` — the last thing a root's own thread writes.
    fn closing_bookend(&self, id: &AgentId) -> (ExitStatus, String) {
        let events = self.project.agent(id).events();
        wait_until("the node's thread to close its stream", || {
            let (_, events) = EventReader::open_path(&events).ok()?;
            events.into_iter().find_map(|e| match e.payload {
                Payload::Lifecycle(Lifecycle::Exited { status, exit }) => {
                    Some((status, exit.description))
                }
                _ => None,
            })
        })
    }

    /// The contract file persisted for `id`, parsed.
    fn contract_of(&self, id: &AgentId) -> TaskContract {
        wait_until("the child's thread to persist its contract", || {
            let all = persisted_contracts(&self.state).ok()?;
            let mine = all
                .into_iter()
                .find(|c| c.path.to_string_lossy().contains(&id.0))?;
            serde_json::from_value(mine.parsed.ok()?).ok()
        })
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

fn record_agent(k: &RecordKind) -> Option<&AgentId> {
    match k {
        RecordKind::SpawnIntent(r) => Some(&r.agent_id),
        RecordKind::Spawned(r) => Some(&r.agent_id),
        RecordKind::StateChanged(r) => Some(&r.agent_id),
        RecordKind::Exited(r) => Some(&r.agent_id),
        RecordKind::SpawnAborted(r) => Some(&r.agent_id),
        RecordKind::KillIntent(r) => Some(&r.agent_id),
        RecordKind::KillConfirmed(r) => Some(&r.agent_id),
        RecordKind::ContractPersisted(r) => Some(&r.agent_id),
        _ => None,
    }
}

fn count(records: &[RecordKind], pick: impl Fn(&RecordKind) -> bool) -> usize {
    records.iter().filter(|k| pick(k)).count()
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

fn dead(pid: i32) -> bool {
    matches!(liveness(pid), Liveness::Gone | Liveness::Zombie)
}

/// A root the supervisor owns, killed over the socket while another root keeps running.
#[test]
fn a_running_root_killed_over_the_socket_ends_cancelled_and_stays_cancelled() {
    let bed = Bed::new("node-kill-root");
    let victim = bed.spawn_root(VICTIM_ROOT);
    let survivor = bed.spawn_root(SURVIVOR_ROOT);
    let (leader, descendant) = bed.pids_of(VICTIM_ROOT);
    let (survivor_leader, _) = bed.pids_of(SURVIVOR_ROOT);
    // Negative control: a kill of processes that were already gone would pass every check below.
    assert_eq!(liveness(leader), Liveness::Alive);
    assert_eq!(liveness(descendant), Liveness::Alive);

    let state = bed.kill(&victim).expect("a running root is ended");
    assert_eq!(state, NodeState::Exited(ExitStatus::Cancelled));
    assert!(dead(leader), "the node's own process is dead");
    assert!(
        dead(descendant),
        "the node's descendant is dead too — the kill reached the tree, not just the leader"
    );

    // Ending one node is not quitting: the other root, its process and the supervisor carry on.
    assert_eq!(bed.state_over_socket(&survivor), NodeState::Running);
    assert_eq!(liveness(survivor_leader), Liveness::Alive);

    // **Only after the victim's own thread has written its last record** is the journal read.
    let (status, description) = bed.closing_bookend(&victim);
    assert_eq!(
        status,
        ExitStatus::Cancelled,
        "the stream closes as the kill"
    );
    assert!(description.contains("kill"), "{description}");
    let about = bed.records_about(&victim);
    assert_eq!(
        count(&about, |k| matches!(k, RecordKind::KillIntent(_))),
        1,
        "{about:?}"
    );
    assert_eq!(
        count(&about, |k| matches!(k, RecordKind::KillConfirmed(_))),
        1,
        "{about:?}"
    );
    assert_eq!(
        count(&about, |k| matches!(
            k,
            RecordKind::Exited(_) | RecordKind::SpawnAborted(_)
        )),
        0,
        "the thread wrote a second terminal record over the kill: {about:?}"
    );
    assert_eq!(
        bed.state_over_socket(&victim),
        NodeState::Exited(ExitStatus::Cancelled),
        "still Cancelled after the node's thread finished"
    );

    // A second kill of the ended node is refused by name, and signals nothing.
    let e = bed
        .kill(&victim)
        .expect_err("an ended node has nothing left to end");
    assert!(e.contains("already ended"), "{e}");
    assert_eq!(liveness(survivor_leader), Liveness::Alive);
}

/// A child the supervisor owns, killed over the socket under a root that keeps running. The child's
/// thread also writes its contract, and that contract must say what the journal says.
#[test]
fn a_running_child_killed_over_the_socket_ends_cancelled_in_its_journal_and_its_contract() {
    let bed = Bed::new("node-kill-child");
    let root = bed.spawn_root(SURVIVOR_ROOT);
    let (root_leader, _) = bed.pids_of(SURVIVOR_ROOT);
    let child = bed.spawn_child(&root, VICTIM_CHILD);
    let (leader, descendant) = bed.pids_of(VICTIM_CHILD);
    assert_eq!(liveness(leader), Liveness::Alive);
    assert_eq!(liveness(descendant), Liveness::Alive);

    let state = bed.kill(&child).expect("a running child is ended");
    assert_eq!(state, NodeState::Exited(ExitStatus::Cancelled));
    assert!(dead(leader));
    assert!(dead(descendant));
    assert_eq!(bed.state_over_socket(&root), NodeState::Running);
    assert_eq!(liveness(root_leader), Liveness::Alive);

    // The thread's last journal write is `ContractPersisted`, which follows the contract file and
    // the `Exited` it would have written; waiting for it is waiting for the thread to be done.
    let persisted = wait_until("the child's thread to journal its contract", || {
        bed.records_about(&child).into_iter().find_map(|k| match k {
            RecordKind::ContractPersisted(c) => Some(c.status),
            _ => None,
        })
    });
    assert_eq!(persisted, Some(ExitStatus::Cancelled));
    let contract = bed.contract_of(&child);
    let completion = contract
        .completion
        .expect("a finished child has a completion");
    assert_eq!(
        completion.status,
        ExitStatus::Cancelled,
        "the contract agrees with the journal: {}",
        completion.exit.description
    );
    assert!(
        completion.evidence.is_empty(),
        "nothing is verified over a killed node's workspace"
    );
    let about = bed.records_about(&child);
    assert_eq!(
        count(&about, |k| matches!(k, RecordKind::KillConfirmed(_))),
        1,
        "{about:?}"
    );
    assert_eq!(
        count(&about, |k| matches!(
            k,
            RecordKind::Exited(_) | RecordKind::SpawnAborted(_)
        )),
        0,
        "the thread wrote a second terminal record over the kill: {about:?}"
    );
    assert_eq!(
        bed.state_over_socket(&child),
        NodeState::Exited(ExitStatus::Cancelled),
        "still Cancelled after the child's thread finished"
    );
}
