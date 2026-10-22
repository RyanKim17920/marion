//! **§2's `node/cancel`, end to end, one cell per lane**: a real node driven by the real
//! supervisor, held mid-turn, cancelled over the socket. Each cell checks what the row's abort verb
//! promises:
//!
//! * `KillConfirmed.signal` is `None` where the row's verb is the channel's (the node closed its
//!   turn inside its grace), and `Some(9)` where it is `None` (killed at once);
//! * the node ends `Cancelled`, in the journal and in its contract;
//! * the partial work it wrote before the cancel is in its contract's `changed_paths`;
//! * nothing it started survives — its leader and a descendant in its group are both gone.
//!
//! # The lanes, and the rows they stand for
//!
//! A cell is a lane, not a harness: what `node/cancel` does to a node is decided by the driver its
//! row's surfaces select and the verb the row states, and each row's pair is pinned beside the
//! driver tests (`rpc`, `duplex`, `paste`) and by `handler::cancel::tests`'s table. The lanes here:
//!
//! | cell | row | lane | verb |
//! |---|---|---|---|
//! | `codex` | codex | app-server (`turn/interrupt`) | `Channel{6000}` |
//! | `acp-opencode` | acp | ACP (`session/cancel`) | `Channel{5000}` |
//! | `opencode` | opencode | launch-only | `None` |
//!
//! The other launch-only rows (copilot, goose, cline, qwen, hermes, antigravity) take the
//! `opencode` cell's lane with the same `None` verb. pi's JSONL `abort` is the duplex driver's,
//! driven over pipes in `duplex::tests`; claude's stream-json is `None` until probe P-cancel-1.
//!
//! # The bed
//!
//! A detached supervisor with shims named `codex` and `opencode` on its `PATH`, as `node_kill.rs`
//! builds one: `codex` is the app-server shim (`common::app_server`), `opencode` answers
//! `--version`, runs a launch-only node as itself, and runs `opencode acp` as a fake ACP agent
//! that answers `session/cancel` the way the protocol says. Every turn blocks on a gate file the
//! test holds shut, with one descendant in its own process group, and records
//! `<leader pid> <descendant pid>` under the marker its prompt carries.
//!
//! ```sh
//! cargo test -p marion-supervisor --test it_canned cancel_matrix::
//! ```
//!
//! It needs no harness binary, no network and no credential.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marion_core::contract::{AgentId, ExitStatus, TaskContract};
use marion_core::journal::RecordKind;
use marion_core::node::NodeState;
use marion_core::paths::ProjectDir;
use marion_core::proto::params::{AgentSpawnParams, NodeCancelParams, NodeResumeParams};
use marion_core::proto::result::{NodeCancelResult, NodeResumeResult};
use marion_core::proto::{Call, Method, MethodResult, SpawnCaller};
use marion_supervisor::socket::project_root;
use marion_testsupport::{
    Liveness, Scratch, fixture_repo, liveness, persisted_contracts, scratch, survivors, sweep,
};
use serde_json::Value;

use crate::common;
use common::Supervisor;

/// A bound that exists only to fail: every wait here is on work the test has already caused.
const BOUND: Duration = Duration::from_secs(60);

/// The shims' own life cap, so no bad run strands one. Far longer than any passing test.
const SHIM_LIFE: Duration = Duration::from_secs(90);

/// The parent every cell's child is spawned under: a codex root that holds until the gate opens.
const ROOT: &str = "CM-ROOT-4b1d";

/// The turn every node runs: block on the gate with a descendant in the node's group, record both
/// pids under the prompt's marker, and — for a child, which has a worktree of its own — write a
/// file first, the partial work a cancel must keep. `$1` is the turn's text, `$2` the leader's pid.
fn block_script(gate: &Path, pids: &Path) -> String {
    let ticks = SHIM_LIFE.as_millis() / 50;
    format!(
        r#"#!/bin/sh
name=$(printf '%s' "$1" | grep -o 'CM-[A-Z0-9-]*-4b1d' | head -n 1)
[ -n "$name" ] || name=other
case "$name" in
  *CHILD*|*MID*|*LEAF*) mkdir -p src && echo "partial work of $name" > src/partial.txt ;;
esac
( waited=0
  while [ ! -e {gate} ] && [ "$waited" -le {ticks} ]; do sleep 0.05; waited=$((waited + 1)); done ) &
mkdir -p {pids}
echo "$2 $!" > {pids}/$name.tmp && mv {pids}/$name.tmp {pids}/$name
waited=0
while [ ! -e {gate} ] && [ "$waited" -le {ticks} ]; do
  sleep 0.05
  waited=$((waited + 1))
done
exit 0
"#,
        gate = common::shell_quote(gate),
        pids = common::shell_quote(pids),
    )
}

/// `opencode acp`, as the protocol's minimum: the handshake, a session whose `session/new` it
/// writes to `decl/<agent id>.json` (where the test reads the node's token from), a prompt that runs
/// the block script, and `session/cancel` answered with the pending prompt's
/// `stopReason: "cancelled"`. Every other request is answered `{}`. stdin EOF ends it.
const ACP_AGENT: &str = r#"#!/usr/bin/env python3
import json, os, subprocess, sys
block, decl = sys.argv[1], sys.argv[2]
def out(o):
    o["jsonrpc"] = "2.0"
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
def agent_id(v):
    if isinstance(v, dict):
        if v.get("name") == "MARION_AGENT_ID" and isinstance(v.get("value"), str):
            return v["value"]
        if isinstance(v.get("MARION_AGENT_ID"), str):
            return v["MARION_AGENT_ID"]
        v = list(v.values())
    if isinstance(v, list):
        for x in v:
            found = agent_id(x)
            if found:
                return found
    return None
pending = None
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    meth, i = m.get("method"), m.get("id")
    if meth == "initialize":
        out({"id": i, "result": {"protocolVersion": 1, "agentInfo": {"name": "OpenCode", "version": "1.18.32"}, "agentCapabilities": {"loadSession": True}}})
    elif meth == "session/new":
        os.makedirs(decl, exist_ok=True)
        with open(os.path.join(decl, (agent_id(m) or "unknown") + ".json"), "w") as f:
            f.write(line)
        # The model marion's canned launch selects, already current, and opencode's own mode.
        model = {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "marion/default", "options": [{"value": "marion/default", "name": "default"}]}
        modes = {"currentModeId": "build", "availableModes": [{"id": "build", "name": "build"}]}
        out({"id": i, "result": {"sessionId": "cm-session-%d" % os.getpid(), "configOptions": [model], "modes": modes}})
    elif meth == "session/prompt":
        pending = i
        text = "".join(p.get("text", "") for p in m["params"]["prompt"])
        subprocess.Popen(["/bin/sh", block, text, str(os.getpid())])
    elif meth == "session/cancel":
        if pending is not None:
            out({"id": pending, "result": {"stopReason": "cancelled"}})
            pending = None
    elif i is not None and meth is not None:
        out({"id": i, "result": {}})
"#;

fn write_executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The shims, in `bin`: `codex` (app-server) and `opencode` (launch-only, and `acp`).
fn shims(dir: &Path, bin: &Path, gate: &Path, pids: &Path, decl: &Path) {
    let slow = common::shell_quote(&dir.join("slow"));
    let block = dir.join("block.sh");
    write_executable(&block, &block_script(gate, pids));
    let acp = dir.join("acp-agent.py");
    write_executable(&acp, ACP_AGENT);
    let block_q = common::shell_quote(&block);
    common::app_server::fake_codex(
        bin,
        &format!(
            "case \"$1\" in --version) [ -e {slow} ] && sleep 2; echo \"codex-cli 0.146.0-marion-shim\"; exit 0 ;; esac\n\
             exec /bin/sh {block_q} \"$*\" \"$PPID\"\n"
        ),
    );
    write_executable(
        &bin.join("opencode"),
        &format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
               --version) echo 1.18.32; exit 0 ;;\n\
               acp) exec python3 {acp} {block_q} {decl} ;;\n\
             esac\n\
             exec /bin/sh {block_q} \"$*\" \"$$\"\n",
            acp = common::shell_quote(&acp),
            decl = common::shell_quote(decl),
        ),
    );
}

struct Bed {
    _scratch: Scratch,
    repo: PathBuf,
    state: PathBuf,
    project: ProjectDir,
    gate: PathBuf,
    pids: PathBuf,
    decl: PathBuf,
    /// While it exists, the `codex` shim's `--version` probe takes two seconds: a launch held
    /// between its decision and its process.
    slow: PathBuf,
    needle: String,
    key: PathBuf,
    path_env: String,
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
        let decl = dir.join("decl");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        shims(&dir, &bin, &gate, &pids, &decl);
        // The whole search path is stated, so no real harness can be what runs.
        for sys in ["/usr/bin", "/bin"] {
            for h in ["codex", "opencode"] {
                assert!(
                    !Path::new(sys).join(h).exists(),
                    "{sys} holds a `{h}`, so this bed could launch a real harness"
                );
            }
        }
        let key = project_root(&repo);
        // `python3` for the shims, from wherever this machine keeps it.
        let python = which("python3");
        let path_env = format!("{}:{python}:/usr/bin:/bin", bin.to_string_lossy());
        let supervisor = start_supervisor(&state, &key, &path_env);
        Self {
            needle: dir.to_string_lossy().to_string(),
            project: ProjectDir::new(&state, &key),
            _scratch: s,
            repo,
            state,
            gate,
            pids,
            decl,
            slow: dir.join("slow"),
            key,
            path_env,
            supervisor,
        }
    }

    /// SIGKILL the supervisor and start another over the same journal: every node it held is an
    /// orphan, its process still running.
    fn restart_supervisor(&mut self) {
        self.supervisor.stop();
        self.supervisor = start_supervisor(&self.state, &self.key, &self.path_env);
    }

    fn resume(&self, id: &AgentId, prompt: &str) -> Result<NodeResumeResult, String> {
        let answered = self.supervisor.call(Call::NodeResume(NodeResumeParams {
            agent_id: id.clone(),
            prompt: prompt.into(),
        }))?;
        let MethodResult::NodeResume(r) = Method::NodeResume.decode_result(&answered).unwrap()
        else {
            panic!("node/resume answers with a node/resume result");
        };
        Ok(r)
    }

    fn try_cancel(&self, id: &AgentId) -> Result<NodeCancelResult, String> {
        let answered = self.supervisor.call(Call::NodeCancel(NodeCancelParams {
            agent_id: id.clone(),
            caller: None,
        }))?;
        let MethodResult::NodeCancel(r) = Method::NodeCancel.decode_result(&answered).unwrap()
        else {
            panic!("node/cancel answers with a node/cancel result");
        };
        Ok(r)
    }

    fn spawn(&self, agent_type: &str, marker: &str, caller: Option<SpawnCaller>) -> AgentId {
        let child = caller.is_some();
        let answered = self
            .supervisor
            .call(Call::AgentSpawn(AgentSpawnParams {
                // The operator's opt-in: a sandboxed codex root starts unsandboxed opencode nodes.
                wider_children: (!child).then_some(true),
                budget_tokens: None,
                review_of: None,
                notify_parent: false,
                agent_type: agent_type.into(),
                prompt: format!("{marker}: hold until the gate opens"),
                native_launch: None,
                caller,
                repo: (!child).then(|| self.repo.clone()),
                acceptance_criteria: vec![],
                verification: if child { vec!["true".into()] } else { vec![] },
                writable_scope: if child { vec!["src/**".into()] } else { vec![] },
                timeout_secs: Some(SHIM_LIFE.as_secs() + 30),
                model: None,
                no_change_record: (!child).then_some(true),
                pane: None,
                isolation: None,
                allow_concurrent_writes: None,
                profile: None,
                candidates: vec![],
                race: None,
            }))
            .unwrap_or_else(|e| panic!("agent/spawn of {agent_type} must be served: {e}"));
        let MethodResult::AgentSpawn(r) = Method::AgentSpawn.decode_result(&answered).unwrap()
        else {
            panic!("agent/spawn answers with an agent/spawn result");
        };
        r.agent_id
    }

    fn root(&self, agent_type: &str, marker: &str) -> AgentId {
        self.spawn(agent_type, marker, None)
    }

    /// A child of `parent`, proved by the token its declaration names: a file marion wrote under
    /// the state directory, or — for an ACP parent, whose declaration rides `session/new` — the
    /// frame the fake agent kept.
    fn child(&self, parent: &AgentId, agent_type: &str, marker: &str) -> AgentId {
        let token = self.token_of(parent);
        self.spawn(
            agent_type,
            marker,
            Some(SpawnCaller {
                agent_id: parent.clone(),
                node_token: token.into(),
            }),
        )
    }

    fn token_of(&self, id: &AgentId) -> String {
        let frame = self.decl.join(format!("{}.json", id.0));
        let text = wait_until("the node's session to open, or its declaration", || {
            std::fs::read_to_string(&frame).ok().or_else(|| {
                common::walk_finds(
                    &self.state,
                    &id.0,
                    marion_harness::mcp_bridge::NODE_TOKEN_FILE_ENV,
                )
                .then(String::new)
            })
        });
        if text.is_empty() {
            return common::node_token_of(&common::declaration_of(&self.state, id));
        }
        let v: Value = serde_json::from_str(&text).unwrap();
        let file = env_value(&v, marion_harness::mcp_bridge::NODE_TOKEN_FILE_ENV)
            .expect("the session's declaration names a token file");
        std::fs::read_to_string(file).unwrap().trim().to_string()
    }

    fn cancel(&self, id: &AgentId) -> NodeCancelResult {
        self.try_cancel(id)
            .unwrap_or_else(|e| panic!("node/cancel must be served: {e}"))
    }

    /// `(leader, descendant)` for the node whose prompt carried `marker`, once its turn wrote them.
    fn pids_of(&self, marker: &str) -> (i32, i32) {
        let path = self.pids.join(marker);
        let text = wait_until(&format!("{marker}'s turn to record its pids"), || {
            std::fs::read_to_string(&path).ok()
        });
        let mut it = text.split_whitespace().map(|p| p.parse::<i32>().unwrap());
        (it.next().unwrap(), it.next().unwrap())
    }

    /// The `KillConfirmed` the cancel wrote for `id`: its signal and description.
    fn confirmed(&self, id: &AgentId) -> (Option<i32>, String) {
        let found: Vec<_> = common::journal::records(&self.project.journal())
            .into_iter()
            .filter_map(|k| match k {
                RecordKind::KillConfirmed(c) if &c.agent_id == id => {
                    Some((c.exit.signal, c.exit.description))
                }
                _ => None,
            })
            .collect();
        assert_eq!(found.len(), 1, "one confirmation for {}: {found:?}", id.0);
        found.into_iter().next().unwrap()
    }

    /// The contract `id`'s thread persisted, once it journaled it.
    fn contract_of(&self, id: &AgentId) -> TaskContract {
        let status = wait_until("the child's thread to journal its contract", || {
            common::journal::records(&self.project.journal())
                .into_iter()
                .find_map(|k| match k {
                    RecordKind::ContractPersisted(c) if &c.agent_id == id => Some(c.status),
                    _ => None,
                })
        });
        assert_eq!(status, Some(ExitStatus::Cancelled));
        wait_until("the child's contract file", || {
            let mine = persisted_contracts(&self.state)
                .ok()?
                .into_iter()
                .find(|c| c.path.to_string_lossy().contains(&id.0))?;
            serde_json::from_value(mine.parsed.ok()?).ok()
        })
    }

    /// Whether the node's recorded stream holds `frame` — the harness's own word that its turn
    /// closed, which only the verb could have drawn from it. Read with spacing and the escaping of
    /// a line quoted inside an event set aside: the shims write Python's `json.dumps`.
    fn stream_holds(&self, id: &AgentId, frame: &str) -> bool {
        let bare = |t: &str| t.replace(['\\', ' '], "");
        std::fs::read_to_string(self.project.agent(id).events())
            .is_ok_and(|t| bare(&t).contains(&bare(frame)))
    }

    /// **One cell's checks** for a child cancelled on its own: the verb's signal, `Cancelled`, the
    /// partial work kept, and nothing left running. For a graceful cell, [`Graceful`] names the
    /// frame that says marion has read the turn's start, and the one the harness answers the row's
    /// channel verb with.
    fn assert_cell(&self, id: &AgentId, marker: &str, graceful: Option<Graceful>) {
        if let Some(g) = graceful {
            // The driver has read the turn's start once the stream holds it, so a verb that names
            // the turn has one to name. Causal, not timed: a loaded reader thread lags the pipe.
            assert!(
                marion_testsupport::until(|| self.stream_holds(id, g.started)),
                "{marker}: marion never recorded `{}`",
                g.started
            );
        }
        let closed = graceful.map(|g| g.closed);
        let graceful = closed.is_some();
        let (leader, descendant) = self.pids_of(marker);
        // Negative control: a cancel of processes already gone would pass every check below.
        assert_eq!(liveness(leader), Liveness::Alive, "{marker}");
        assert_eq!(liveness(descendant), Liveness::Alive, "{marker}");

        let started = Instant::now();
        let r = self.cancel(id);
        assert_eq!(
            r.state,
            NodeState::Exited(ExitStatus::Cancelled),
            "{marker}"
        );
        assert_eq!(
            r.nodes
                .iter()
                .map(|n| (&n.agent_id, n.forced))
                .collect::<Vec<_>>(),
            [(id, !graceful)],
            "{marker}"
        );
        let (signal, description) = self.confirmed(id);
        if let Some(frame) = closed {
            assert_eq!(signal, None, "{marker}: {description}");
            assert!(
                marion_testsupport::until(|| self.stream_holds(id, frame)),
                "{marker}: the turn was not closed by the verb; no `{frame}` in its stream"
            );
            assert!(
                started.elapsed() < Duration::from_millis(6_000 + 2_000),
                "{marker}: inside its grace: {:?}",
                started.elapsed()
            );
        } else {
            assert_eq!(signal, Some(9), "{marker}: {description}");
            assert!(
                description.contains("no abort verb"),
                "{marker}: {description}"
            );
        }
        assert!(
            marion_testsupport::until(|| dead(leader) && dead(descendant)),
            "{marker}: something the node started survived: {:?} {:?}",
            liveness(leader),
            liveness(descendant)
        );
        let completion = self
            .contract_of(id)
            .completion
            .expect("a finished child has a completion");
        assert_eq!(completion.status, ExitStatus::Cancelled, "{marker}");
        assert!(
            completion
                .changed_paths
                .iter()
                .any(|p| p == Path::new("src/partial.txt")),
            "{marker}: the partial work is kept: {:?}",
            completion.changed_paths
        );
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

/// A graceful cell's two frames: the one that shows marion read the turn's start, and the one the
/// harness closes the turn with on the row's verb.
#[derive(Clone, Copy)]
struct Graceful {
    started: &'static str,
    closed: &'static str,
}

fn start_supervisor(state: &Path, key: &Path, path_env: &str) -> Supervisor {
    Supervisor::start_unsandboxed(
        state,
        key,
        path_env,
        // Nothing reaches it: every harness invocation is a shim.
        "http://127.0.0.1:8099/v1",
        Duration::from_secs(600),
    )
}

/// The value of `name` in a declaration's environment, as ACP writes it (`{name, value}` pairs)
/// or as a map.
fn env_value(v: &Value, name: &str) -> Option<String> {
    match v {
        Value::Object(o) => {
            if o.get("name").and_then(Value::as_str) == Some(name) {
                return o.get("value").and_then(Value::as_str).map(str::to_string);
            }
            if let Some(s) = o.get(name).and_then(Value::as_str) {
                return Some(s.to_string());
            }
            o.values().find_map(|x| env_value(x, name))
        }
        Value::Array(a) => a.iter().find_map(|x| env_value(x, name)),
        _ => None,
    }
}

/// The directory holding `program`, found on this test's own `PATH`.
fn which(program: &str) -> String {
    std::env::var_os("PATH")
        .and_then(|p| std::env::split_paths(&p).find(|d| d.join(program).is_file()))
        .unwrap_or_else(|| panic!("no `{program}` on PATH"))
        .to_string_lossy()
        .into_owned()
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

/// **codex's app-server**: `turn/interrupt` closes the turn, marion kills its command, and the
/// server leaves on stdin EOF — inside the row's 6 s, with no signal of the cancel's own.
#[test]
fn codex_app_server_closes_its_turn_inside_its_grace() {
    let bed = Bed::new("cancel-matrix-codex");
    let root = bed.root("codex", ROOT);
    bed.pids_of(ROOT);
    let child = bed.child(&root, "codex-impl", "CM-CODEX-CHILD-4b1d");
    bed.assert_cell(
        &child,
        "CM-CODEX-CHILD-4b1d",
        Some(Graceful {
            started: r#""method":"turn/started""#,
            closed: r#""status":"interrupted""#,
        }),
    );
}

/// **An ACP agent**: `session/cancel` is answered `cancelled`, and the agent leaves on stdin EOF —
/// inside the row's 5 s, with no signal of the cancel's own.
#[test]
fn an_acp_agent_answers_session_cancel_inside_its_grace() {
    let bed = Bed::new("cancel-matrix-acp");
    let root = bed.root("codex", ROOT);
    bed.pids_of(ROOT);
    let child = bed.child(&root, "acp-opencode", "CM-ACP-CHILD-4b1d");
    bed.assert_cell(
        &child,
        "CM-ACP-CHILD-4b1d",
        Some(Graceful {
            started: r#""sessionId":"cm-session-"#,
            closed: r#""stopReason":"cancelled""#,
        }),
    );
}

/// **A launch-only row** (`None`): nothing can reach the turn after launch, so the cancel kills
/// it at once — and still keeps what it wrote.
#[test]
fn a_launch_only_node_is_killed_at_once_and_keeps_its_partial_work() {
    let bed = Bed::new("cancel-matrix-launch-only");
    let root = bed.root("codex", ROOT);
    bed.pids_of(ROOT);
    let child = bed.child(&root, "opencode", "CM-OPENCODE-CHILD-4b1d");
    bed.assert_cell(&child, "CM-OPENCODE-CHILD-4b1d", None);
}

/// **An ACP tree cancelled at its root ends bottom-up, every level graceful**: the grandchild,
/// then the child, then the root, each sent `session/cancel` and none signalled — and every
/// process in the tree is gone.
#[test]
fn an_acp_tree_cancelled_at_its_root_ends_bottom_up_without_a_signal() {
    let bed = Bed::new("cancel-matrix-acp-tree");
    let root = bed.root("acp-opencode", "CM-ACP-ROOT-4b1d");
    let mid = bed.child(&root, "acp-opencode", "CM-ACP-MID-4b1d");
    let leaf = bed.child(&mid, "acp-opencode", "CM-ACP-LEAF-4b1d");
    let pids: Vec<(i32, i32)> = ["CM-ACP-ROOT-4b1d", "CM-ACP-MID-4b1d", "CM-ACP-LEAF-4b1d"]
        .iter()
        .map(|m| bed.pids_of(m))
        .collect();
    for (leader, descendant) in &pids {
        assert_eq!(liveness(*leader), Liveness::Alive);
        assert_eq!(liveness(*descendant), Liveness::Alive);
    }

    let r = bed.cancel(&root);
    assert_eq!(r.state, NodeState::Exited(ExitStatus::Cancelled));
    assert_eq!(
        r.nodes
            .iter()
            .map(|n| (n.agent_id.clone(), n.forced))
            .collect::<Vec<_>>(),
        [
            (leaf.clone(), false),
            (mid.clone(), false),
            (root.clone(), false)
        ],
        "deepest first, none forced"
    );
    for id in [&leaf, &mid, &root] {
        let (signal, description) = bed.confirmed(id);
        assert_eq!(signal, None, "{}: {description}", id.0);
    }
    assert!(
        marion_testsupport::until(|| pids.iter().all(|(l, d)| dead(*l) && dead(*d))),
        "something in the tree survived: {:?}",
        pids.iter()
            .map(|(l, d)| (liveness(*l), liveness(*d)))
            .collect::<Vec<_>>()
    );
    for id in [&leaf, &mid] {
        assert_eq!(
            bed.contract_of(id).completion.map(|c| c.status),
            Some(ExitStatus::Cancelled),
            "{}",
            id.0
        );
    }
}

/// **A cancel that lands while a resume relaunches the node ends the relaunch.** The resume kills
/// the orphan's surviving process and starts a new one; a cancel asked for between the two waits
/// for the new process rather than reading the node in between — where it is already dead, and
/// its journaled pid the one the resume just ended — and ends the process the resume started. The
/// relaunch is held between its decision and its process (a slow `--version`), so the cancel
/// arrives exactly in that window. Exactly one process is journaled dead by each: the orphan's by
/// the resume, the relaunch's by the cancel, after its `Spawned`.
#[test]
fn a_cancel_during_a_resume_ends_the_relaunched_process() {
    let mut bed = Bed::new("cancel-matrix-resume");
    let root = bed.root("codex", "CM-ORPHAN-4b1d");
    let (old_leader, old_descendant) = bed.pids_of("CM-ORPHAN-4b1d");
    wait_until("the root's session to be journaled", || {
        common::journal::records(&bed.project.journal())
            .into_iter()
            .any(|k| matches!(k, RecordKind::SessionObserved(s) if s.agent_id == root))
            .then_some(())
    });
    bed.restart_supervisor();
    assert_eq!(
        liveness(old_leader),
        Liveness::Alive,
        "an orphan, still running"
    );

    std::fs::write(&bed.slow, b"").unwrap();
    let journal = bed.project.journal();
    let (resumed, cancelled) = std::thread::scope(|s| {
        let resume = s.spawn(|| bed.resume(&root, "CM-RESUMED-4b1d: carry on"));
        // The resume has ended the orphan's process and is starting its own.
        wait_until("the resume to kill the orphan's process", || {
            common::journal::records(&journal)
                .into_iter()
                .any(|k| matches!(k, RecordKind::KillConfirmed(c) if c.agent_id == root))
                .then_some(())
        });
        let cancelled = bed.try_cancel(&root);
        (resume.join().unwrap(), cancelled)
    });
    let resumed = resumed.expect("the orphan relaunches");
    assert_eq!(resumed.spawn_generation, 2);
    let cancelled = cancelled.expect("the cancel reaches the relaunched node");
    assert_eq!(cancelled.state, NodeState::Exited(ExitStatus::Cancelled));
    // Whether the relaunch was still coming up — its port not yet attached, so killed at once — or
    // already in its turn and interrupted is the scheduler's; that the cancel ended it is not.
    assert_eq!(
        cancelled
            .nodes
            .iter()
            .map(|n| &n.agent_id)
            .collect::<Vec<_>>(),
        [&root]
    );

    // The relaunch's leader, off its own `Spawned`: a relaunch cancelled while it was still coming
    // up never reached the turn that would have recorded it, nor started a descendant.
    let spawned: Vec<i32> = common::journal::records(&journal)
        .into_iter()
        .filter_map(|k| match k {
            RecordKind::Spawned(sp) if sp.agent_id == root => sp.pid,
            _ => None,
        })
        .collect();
    let new_leader = *spawned.last().unwrap();
    assert_ne!(new_leader, old_leader, "a second process");
    let new_descendant = std::fs::read_to_string(bed.pids.join("CM-RESUMED-4b1d"))
        .ok()
        .and_then(|t| t.split_whitespace().nth(1)?.parse::<i32>().ok());
    assert!(
        marion_testsupport::until(|| [old_leader, old_descendant, new_leader]
            .into_iter()
            .chain(new_descendant)
            .all(dead)),
        "a process of either life survived"
    );
    // The orphan's death is the resume's, and the relaunch's is the cancel's, after its `Spawned`.
    let about: Vec<RecordKind> = common::journal::records(&journal)
        .into_iter()
        .filter(|k| match k {
            RecordKind::KillConfirmed(c) => c.agent_id == root,
            RecordKind::Spawned(sp) => sp.agent_id == root,
            _ => false,
        })
        .collect();
    let shape: Vec<(&str, Option<i32>)> = about
        .iter()
        .map(|k| match k {
            RecordKind::Spawned(sp) => ("Spawned", sp.pid),
            RecordKind::KillConfirmed(c) => ("KillConfirmed", c.exit.signal.filter(|_| false)),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        shape,
        [
            ("Spawned", Some(old_leader)),
            ("KillConfirmed", None),
            ("Spawned", Some(new_leader)),
            ("KillConfirmed", None),
        ],
        "one death per life, the relaunch's after its own `Spawned`: {about:?}"
    );
    let RecordKind::KillConfirmed(by_resume) = &about[1] else {
        unreachable!()
    };
    assert!(
        by_resume.exit.description.contains("before resuming"),
        "{by_resume:?}"
    );
}

/// **Two resumes of one orphan relaunch it once.** The second waits behind the first's decision
/// and, reading the node again under it, finds it running: refused, with nothing signalled — not a
/// kill of the first's relaunch by the orphan's stale record, and not a second process against
/// one transcript.
#[test]
fn two_resumes_of_one_orphan_relaunch_it_once() {
    let mut bed = Bed::new("cancel-matrix-double-resume");
    let root = bed.root("codex", "CM-TWICE-4b1d");
    bed.pids_of("CM-TWICE-4b1d");
    wait_until("the root's session to be journaled", || {
        common::journal::records(&bed.project.journal())
            .into_iter()
            .any(|k| matches!(k, RecordKind::SessionObserved(s) if s.agent_id == root))
            .then_some(())
    });
    bed.restart_supervisor();
    std::fs::write(&bed.slow, b"").unwrap();

    let results = std::thread::scope(|s| {
        let a = s.spawn(|| bed.resume(&root, "CM-TWICE-A-4b1d: carry on"));
        let b = s.spawn(|| bed.resume(&root, "CM-TWICE-B-4b1d: carry on"));
        [a.join().unwrap(), b.join().unwrap()]
    });
    let won: Vec<_> = results.iter().filter(|r| r.is_ok()).collect();
    assert_eq!(won.len(), 1, "exactly one relaunch: {results:?}");
    let spawns = common::journal::records(&bed.project.journal())
        .into_iter()
        .filter(|k| matches!(k, RecordKind::Spawned(sp) if sp.agent_id == root))
        .count();
    assert_eq!(spawns, 2, "the first life and one relaunch");
    let kills = common::journal::records(&bed.project.journal())
        .into_iter()
        .filter(|k| matches!(k, RecordKind::KillConfirmed(c) if c.agent_id == root))
        .count();
    assert_eq!(kills, 1, "only the orphan's process was killed");
}
