//! **A native root delegates** — M3's native facade and §5.4's capability, in one run.
//!
//! `native_facade_e2e.rs` proves that the operator's own harness runs through the relay as a
//! journaled node. This proves the consequence that makes putting marion's MCP server in front of
//! it worth anything: the harness can call `spawn`, the supervisor authorizes it as that node, and
//! the child comes back to the native root as its own tool result.
//!
//! It is `m1_hop.rs`'s hop with the root replaced: there the root is `marion run claude`, here it
//! is the **shipped `marion claude`** on the operator's own controlling PTY. Everything else is
//! the same shape, deliberately — the same canned provider, the same wire separation (an Anthropic
//! root and a Responses child, so no marker is needed to tell whose turn a request is), the same
//! `TaskContract` read back off the request the root sent after its `spawn` returned.
//!
//! **No tokens are spent.** Both nodes talk to `CannedServer` on loopback: the child because the
//! supervisor compiles marion's own canned provider into its declaration, and the **root** because
//! the operator's environment is the native node's environment (`assemble_native` carries the
//! client's `vars_os` minus `MARION_*`), so `ANTHROPIC_BASE_URL` set on the client reaches the
//! harness exactly as it would for an operator who exported it in their shell. That is the
//! operator's own channel, not a test-only production override: a native node is the operator's
//! own login (§6.4) and marion places no provider on its argv or in its environment.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use marion_core::contract::{AgentId, TaskContract};
use marion_core::paths::ProjectDir;
use marion_provider::script::ROOT_TOOL_USE_ID;
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::handler::RegistryHandle;
use marion_supervisor::pty::{PtyHost, PtyMaster, StdinPlan, WinSize, spawn_pty};
use marion_supervisor::registry::{LiveRegistry, Registry};
use marion_supervisor::serve::{NativeLaunchConfig, Server};
use marion_supervisor::socket::own_uid;
use marion_supervisor::socket::{Acquired, SocketPaths, acquire, project_root, socket_paths};
use marion_testsupport::{fixture_repo, on_path, pinned_version, scratch, until_within};
use serde_json::{Value, json};

mod common;
use common::cast::cast_text;
use common::client::Client;
use common::mcp_result::tool_result_text;

/// Every wait here is bounded by this and none is a verdict: a real claude turn, a real codex turn
/// and a `git worktree add` all happen inside it.
const BOUND: Duration = Duration::from_secs(240);

/// The operator's terminal as the client finds it.
const OPERATOR_SIZE: WinSize = WinSize {
    cols: 117,
    rows: 43,
};

fn until(cond: impl FnMut() -> bool) -> bool {
    until_within(BOUND, Duration::from_millis(100), cond)
}

/// A supervisor composed the way `detach.rs` stage 3 composes it, over a one-commit repository,
/// pointed at a canned provider.
struct Bed {
    _work: marion_testsupport::Scratch,
    state: PathBuf,
    repo: PathBuf,
    project_dir: ProjectDir,
    paths: SocketPaths,
    server: Option<Server>,
}

impl Bed {
    fn new(tag: &str, base_url: &str) -> Bed {
        let work = scratch(tag);
        let state = work.join("state");
        // A real repository, because the child this run creates gets a §6.6 worktree of it.
        let repo = fixture_repo(&work);
        let key = project_root(&repo);
        let paths = socket_paths(&state, &key, own_uid());
        assert!(
            paths.overflow().is_none(),
            "this bed's socket must live under <state>; {:?} overflowed to /tmp",
            paths.socket()
        );
        let Acquired::Serving(serving) = acquire(&paths).expect("bind both listeners") else {
            panic!("fresh project unexpectedly dialed an existing supervisor")
        };
        let project_dir = ProjectDir::new(&state, paths.canonical_project());
        let live = Arc::new(LiveRegistry::follow(
            Registry::boot(&project_dir).expect("an absent journal is an empty tree"),
            Duration::from_millis(10),
        ));
        let env = marion_supervisor::run::Env {
            project_dir: project_dir.clone(),
            state: state.clone(),
            project_root: paths.canonical_project().to_path_buf(),
            bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
            // The child's provider, compiled into its declaration by the supervisor.
            base_url: Some(base_url.to_string()),
            auth: marion_harness::Auth::Canned,
        };
        let handle = RegistryHandle::owning(live, env.clone());
        let server = Server::start_with_native_launch(
            serving,
            Arc::clone(&handle),
            NativeLaunchConfig {
                descriptors: marion_core::PRODUCTION_NATIVE_FACADES,
                adapter_for: marion_harness::native_adapter,
                env,
            },
            Duration::from_secs(300),
        )
        .expect("the enabled native bootstrap service installs once");
        Bed {
            _work: work,
            state,
            repo,
            project_dir,
            paths,
            server: Some(server),
        }
    }

    /// The one native root this bed's supervisor spawned, once the journal names it.
    fn native_root(&self) -> AgentId {
        let mut found = None;
        assert!(
            until(|| {
                found = self
                    .replayed_nodes()
                    .into_iter()
                    .find(|node| node.spawn_confirmed && node.parent_id().is_none())
                    .map(|node| node.agent_id.clone());
                found.is_some()
            }),
            "the supervisor never journaled a Spawned native root"
        );
        found.expect("a confirmed native root")
    }

    fn replayed_nodes(&self) -> Vec<marion_core::registry::ReplayedNode> {
        Registry::boot(&self.project_dir)
            .expect("the journal replays")
            .tree()
            .nodes()
            .to_vec()
    }
}

impl Drop for Bed {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.stop();
        }
    }
}

/// The shipped `marion claude <tail>` as a foreground session leader on a fresh PTY marion
/// records, with a provider endpoint in the operator's own environment.
struct Operator {
    host: PtyHost,
    cast: PathBuf,
}

impl Operator {
    fn facade(bed: &Bed, harness: &str, tail: &[&str], env: &[(&str, &str)]) -> Operator {
        let cast = bed.state.join(format!("operator-{harness}.cast"));
        let master = PtyMaster::open(OPERATOR_SIZE).expect("the operator's pty");
        // Held until the client owns a slave: a master whose only slave was opened and closed
        // reads EOF, and the host's reader would exit before the client wrote anything.
        let probe: OwnedFd = master.open_slave().expect("baseline slave");
        let _ = rustix::termios::tcgetattr(probe.as_fd()).unwrap();
        let host = PtyHost::start(
            AgentId("operator".into()),
            master,
            &cast,
            OPERATOR_SIZE,
            "xterm-256color",
            Instant::now(),
        )
        .expect("recording the operator's screen");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_marion"));
        cmd.arg(harness)
            .args(tail)
            .current_dir(&bed.repo)
            .env("MARION_STATE_DIR", &bed.state)
            .env("TERM", "xterm-256color");
        for (key, value) in env {
            cmd.env(key, value);
        }
        let witness = marion_harness::ExecutionSurfaces::opaque()
            .display_plane()
            .expect("the opaque shape declares a display plane");
        let child = spawn_pty(
            witness,
            &mut cmd,
            host.master(),
            StdinPlan::TerminalSlave,
            None,
        )
        .expect("the shipped marion binary starts on the operator's pty");
        host.adopt(child);
        drop(probe);
        Operator { host, cast }
    }

    fn exited(&self) -> bool {
        self.host
            .poll_exited_unreaped()
            .expect("polling the shipped client")
    }

    fn finish(self) -> Option<std::process::ExitStatus> {
        self.host
            .shutdown()
            .expect("shutting the operator's pty down")
    }
}

fn calls_spawn(request: &Value) -> bool {
    request
        .pointer("/body/messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|m| {
                m.get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|blocks| {
                        blocks.iter().any(|b| {
                            b.get("type").and_then(Value::as_str) == Some("tool_use")
                                && b.get("name").and_then(Value::as_str)
                                    == Some("mcp__marion__spawn")
                        })
                    })
            })
        })
}

/// **The whole point of the facade, asserted where each half lands.**
///
/// # Mutations
///
/// * `native_launch::ProductionNativeCommandFactory::bridge_for`, `node_token` back to `None`: the
///   root's own bridge refuses its `spawn` with §5.4's sentence, no child is ever journaled, and
///   the operator's screen carries marion's refusal instead of a contract. This is the defect this
///   test was written for.
/// * `NativeLaunchHandler::authorized`, drop the `RegistryHandle::claim`: the declaration carries a
///   token this supervisor never minted and `resolve_caller` refuses it on the socket.
/// * `NativeNodeJournal::intent` moved after the claim: the registry has no depth to gate on and
///   the spawn is refused as unprojectable.
#[test]
fn a_native_root_delegates_a_child_through_marions_own_mcp_server() {
    for harness in ["claude", "codex"] {
        if !on_path(harness) {
            eprintln!(
                "SKIP: `{harness}` ({}) is not on PATH, so the native delegation hop did not run",
                pinned_version(harness)
            );
            return;
        }
    }

    let script = Script {
        // The root's one call, with an id this test can find its result by.
        root_tool_input: json!({
            "agent_type": "codex-impl",
            "prompt": "Add the marker file under src/ and report back.",
            "acceptance_criteria": ["a file exists under src/ containing the marker"],
            "writable_scope": ["src/**"],
        }),
        ..Script::default()
    };
    let reqlog_dir = scratch("native-spawn-provider");
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: reqlog_dir.join("provider-requests.jsonl"),
        script,
    })
    .expect("the canned provider binds");
    let base_url = server.base_url();

    let bed = Bed::new("native-facade-spawn", &base_url);

    // The operator's own environment, which is the native node's environment. `ANTHROPIC_API_KEY`
    // is blanked beside the token for §6.4's reason: a non-empty key silently wins, and a node
    // that presented the operator's real key to a loopback endpoint is exactly what this suite
    // must never do.
    let op = Operator::facade(
        &bed,
        "claude",
        &[
            "-p",
            "Delegate the marker-file task to a codex child through marion.",
            // marion's native prefix supplies `--mcp-config`; the permission axis and the
            // print-mode shape are the operator's own flags, typed as any operator would.
            "--allowedTools",
            "mcp__marion__spawn",
            "--strict-mcp-config",
            "--setting-sources",
            "",
        ],
        &[
            ("ANTHROPIC_BASE_URL", &base_url),
            ("ANTHROPIC_AUTH_TOKEN", "canned"),
            ("ANTHROPIC_API_KEY", ""),
        ],
    );

    let root = bed.native_root();

    // ---- the child is in this project's tree, under the native root ----------------------------
    //
    // Read through `tree/subscribe` on the project socket — what any client sees — rather than off
    // the journal, because the claim is about the fleet a client can observe.
    let mut client = Client::dial(&bed.paths);
    let mut child = None;
    let delegated = until(|| {
        child = client
            .tree()
            .into_iter()
            .find(|n| n.parent_id.as_ref() == Some(&root));
        child.is_some() || op.exited()
    });
    if child.is_none() && op.exited() {
        let screen = cast_text(&op.cast, "o");
        let nodes = bed.replayed_nodes();
        let status = op.finish();
        panic!(
            "the native root exited ({status:?}) having delegated nothing. What it wrote to the \
             operator's terminal:\n{screen}\njournal: {nodes:?}"
        );
    }
    assert!(
        delegated,
        "no child appeared under the native root {} in `tree/subscribe`: {:?}",
        root.0,
        bed.replayed_nodes()
    );
    let child = child.expect("a child under the native root");
    assert_eq!(child.depth, 1, "one level below the native root");

    // ---- the root received the child's contract as its own tool result -------------------------
    //
    // The far end is the provider's request log: the turn after the `spawn` returned carries the
    // result the harness handed the model, which is `wait`'s delivery at the moment §5.4 makes it.
    assert!(
        until(|| {
            op.exited()
                && server.requests().is_ok_and(|rs| {
                    rs.iter()
                        .any(|r| tool_result_text(r, ROOT_TOOL_USE_ID).is_some())
                })
        }),
        "the native root's `spawn` never came back as a tool result. journal: {:?}\noperator \
         screen:\n{}",
        bed.replayed_nodes(),
        cast_text(&op.cast, "o")
    );
    let requests = server.requests().expect("the request log is readable");
    assert!(
        requests.iter().any(calls_spawn),
        "no recorded request carries the native root's mcp__marion__spawn call"
    );
    let result_text = requests
        .iter()
        .find_map(|r| tool_result_text(r, ROOT_TOOL_USE_ID))
        .expect("a request carries the spawn's tool_result");
    let contract: TaskContract = serde_json::from_str(&result_text).unwrap_or_else(|e| {
        panic!("the tool result does not deserialize to a TaskContract: {e}\n{result_text}")
    });
    assert_eq!(
        contract.requester, root,
        "the contract the native root received must name it as the requester"
    );
    assert_eq!(
        contract.child.harness,
        marion_core::harness::Harness::Codex,
        "the child the native root asked for"
    );

    let status = op.finish();
    assert!(
        status.is_some(),
        "the native client was never reaped; journal: {:?}",
        bed.replayed_nodes()
    );
}
