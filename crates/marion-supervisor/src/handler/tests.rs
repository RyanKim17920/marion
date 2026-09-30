//! Tests for `handler.rs`, moved out of it unchanged.

use super::*;
use crate::native_bootstrap::fakes::{ManualClock, SequenceRng};
use marion_core::contract::{ExitStatus, ProcessExit};
use marion_core::encoding::SystemTime;
use marion_core::harness::Harness;
use marion_core::journal::{
    Exited, JournalRecord, RecordKind, SpawnIntent, Spawned, StateChanged, WriterId, encode,
};
use marion_core::node::BlockReason;
use marion_core::proto::Frame;
use marion_testsupport::{append, scratch, until};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

fn id(s: &str) -> AgentId {
    AgentId(s.into())
}

/// **The child's launch refusal carries the thread's own sentence, as the root's does.** A
/// codex row granting `read` is refused at `compile` with a sentence naming the tool and the
/// harness; the answer to `agent/spawn` must quote it, not replace it with "a worktree, a
/// configuration document, or the harness `--version` probe".
#[test]
fn a_child_launch_failure_quotes_the_reason_the_thread_filed() {
    let why = "compiling the child's launch: codex: no mapping for marion tool `read`; this \
               harness's adapter provides none";
    let err = spawn_failed_before_the_process_existed(&id("a1"), Some(why));
    let msg = err.to_string();
    assert!(msg.contains(why), "the sentence survives: {msg}");
    assert!(msg.contains("a1"), "and the node is named: {msg}");
    let none = spawn_failed_before_the_process_existed(&id("a1"), None).to_string();
    assert!(
        none.contains("filed no reason"),
        "a thread that filed nothing is reported as such: {none}"
    );
}

fn line(seq: u64, ms: u64, kind: RecordKind) -> Vec<u8> {
    encode(&JournalRecord {
        writer: WriterId("w".into()),
        seq,
        ts: SystemTime::from_unix_millis(ms),
        mono_ns: seq,
        provenance: marion_core::ir::Provenance::marion(),
        src_seq: None,
        kind,
    })
    .expect("a record encodes")
}

/// The harness comes from the agent type, as a real writer's would — and the projection reads
/// it back from the **journal**, not from the type, because the journal records what was
/// actually launched.
fn intent(agent: &str, parent: Option<&str>, ty: &str, depth: u32) -> RecordKind {
    RecordKind::SpawnIntent(SpawnIntent {
        budget: None,
        review_of: None,
        agent_id: id(agent),
        parent_id: parent.map(id),
        agent_type: ty.into(),
        harness: agent_type::builtin(ty)
            .map(|t| t.harness)
            .unwrap_or(Harness::Codex),
        depth,
        task_id: None,
        timeout_secs: None,
        verification: vec![],
        race: None,
        workflow: None,
    })
}

fn replay_of(records: &[Vec<u8>]) -> Replay {
    let mut r = Replay::default();
    for b in records {
        r.extend(b);
    }
    r
}

fn node_of(records: &[Vec<u8>], agent: &str) -> ReplayedNode {
    replay_of(records).get(&id(agent)).unwrap().clone()
}

/// **A seat's summary carries its badge, and the verdict arrives with the decision** — with
/// no change to the node's own state, so the badge is one of the facts `Extra` watches.
#[test]
fn a_seat_summary_carries_its_race_badge_and_its_verdict_once_decided() {
    use marion_core::journal::RaceDecided;
    use marion_core::race::{DecidedBy, RaceId, RaceRole, RaceSeat, SeatVerdict};
    let race_id = RaceId("r-1".into());
    let mut seat = intent("s", Some("p"), "claude", 1);
    if let RecordKind::SpawnIntent(i) = &mut seat {
        i.race = Some(RaceSeat {
            race_id: race_id.clone(),
            role: RaceRole::Candidate(2),
        });
    }
    let open = vec![line(0, 1, seat)];
    let badge = summarize(&node_of(&open, "s"), false)
        .unwrap()
        .race
        .unwrap();
    assert_eq!(
        (badge.race_id.clone(), badge.seat, badge.verdict),
        (race_id.clone(), 2, None)
    );

    let mut decided = open.clone();
    decided.push(line(
        1,
        2,
        RecordKind::RaceDecided(RaceDecided {
            race_id,
            winner: Some(id("s")),
            decided_by: DecidedBy::Verification,
            verdicts: vec![(2, SeatVerdict::Won)],
        }),
    ));
    let node = node_of(&decided, "s");
    assert_eq!(
        summarize(&node, false).unwrap().race.unwrap().verdict,
        Some(SeatVerdict::Won)
    );
    let spending = crate::spending::Spending::default();
    assert_ne!(
        Extra::of(&node, &spending),
        Extra::of(&node_of(&open, "s"), &spending)
    );
    assert!(
        summarize(
            &node_of(&[line(0, 1, intent("x", None, "claude", 0))], "x"),
            false
        )
        .unwrap()
        .race
        .is_none()
    );
}

/// **An ended node's row total is the journal's alone** — what a restarted supervisor shows
/// with no live figure and no stream read — even beside a stale live entry; a running node's is
/// its recorded runs (a resume's earlier lives) plus the run in progress its sink published;
/// and once the tree has it ended, its live entry is dropped.
#[test]
fn an_ended_nodes_row_total_is_the_journals_and_a_running_ones_adds_its_live_run() {
    let usage = marion_core::contract::TokenUsage {
        input: 90,
        output: 20,
        cache_read: 10,
        cache_write: 0,
        reasoning: Some(5),
    };
    let recorded = line(
        1,
        2,
        RecordKind::UsageRecorded(marion_core::journal::UsageRecorded {
            agent_id: id("child"),
            usage,
            turns: vec![],
        }),
    );
    let intent = line(0, 1, intent("child", Some("root"), "codex-impl", 1));
    let ended = line(
        2,
        3,
        RecordKind::Exited(marion_core::journal::Exited {
            agent_id: id("child"),
            status: ExitStatus::Ok,
            exit: marion_core::contract::ProcessExit {
                code: Some(0),
                signal: None,
                description: "exited 0".into(),
            },
        }),
    );
    let spending = crate::spending::Spending::default();
    let live = crate::spending::Spent {
        usage: Some(marion_core::contract::TokenUsage {
            input: 7,
            ..Default::default()
        }),
        turns: vec![7],
    };
    spending.publish(&id("child"), live);
    let running = node_of(&[intent.clone(), recorded.clone()], "child");
    assert_eq!(spending.shown_total(&running), Some(127));
    let records = [intent, recorded, ended];
    let n = node_of(&records, "child");
    assert_eq!(
        spending.shown_total(&n),
        Some(120),
        "the stale live run is not added"
    );
    spending.forget_ended(&replay_of(&records));
    assert_eq!(spending.get(&id("child")), None);
    assert_eq!(
        crate::spending::Spending::default().shown_total(&n),
        Some(120),
        "a restarted supervisor, with no live figure at all"
    );
}

/// The projection, on a node the journal fully describes — including the two fields
/// `registry.rs` refused to fabricate.
#[test]
fn a_summary_falls_back_to_the_agent_type_and_names_nothing_it_was_not_told() {
    let n = node_of(
        &[line(0, 1, intent("child", Some("root"), "codex-impl", 1))],
        "child",
    );
    let s = summarize(&n, false).expect("a fully described node projects");
    assert_eq!(s.agent_id, id("child"));
    assert_eq!(s.parent_id, Some(id("root")));
    assert_eq!(s.agent_type, "codex-impl");
    assert_eq!(s.harness, Harness::Codex);
    assert_eq!(s.depth, 1);
    assert_eq!(s.state, NodeState::Spawning);
    assert_eq!(
        s.timeout,
        agent_type::builtin("codex-impl").unwrap().timeout,
        "this intent records no bound — an older journal, or a launch marion timed nothing of \
         — so §3.1's agent-type default is the only thing marion can honestly report"
    );
    assert_eq!(
        s.timeout,
        marion_core::encoding::Duration::from_secs(agent_type::DEFAULT_TIMEOUT_SECS),
        "and that fallback really is §3.1's 900 s default, not a value invented here"
    );
    assert_eq!(
        s.name, None,
        "nothing sets `Node.name` yet, so `None` is what the journal says rather than a \
         placeholder for what marion does not know"
    );
}

/// **A row's clock comes from the journal**: started at its latest `Spawned` (its process's
/// start, which a resume moves), ended at the record that moved it to `Exited`, so a client
/// words elapsed time without the supervisor ticking anything.
#[test]
fn a_summary_carries_when_the_node_started_and_ended() {
    let spawned = |ms| {
        line(
            1,
            ms,
            RecordKind::Spawned(Spawned {
                agent_id: id("c"),
                harness_version: "0.9.0".into(),
                model: None,
                pid: Some(3),
                start_id: None,
                provider: None,
                route: None,
                credential: None,
            }),
        )
    };
    let intent_at = line(0, 1_000, intent("c", None, "codex-impl", 0));
    let running = summarize(&node_of(std::slice::from_ref(&intent_at), "c"), false).unwrap();
    assert_eq!(
        running.started_at,
        Some(SystemTime::from_unix_millis(1_000)),
        "before any Spawned, the intent's time"
    );
    let exited = line(
        2,
        9_000,
        RecordKind::Exited(Exited {
            agent_id: id("c"),
            status: ExitStatus::Ok,
            exit: ProcessExit {
                code: Some(0),
                signal: None,
                description: "done".into(),
            },
        }),
    );
    let s = summarize(&node_of(&[intent_at, spawned(2_000), exited], "c"), false).unwrap();
    assert_eq!(s.started_at, Some(SystemTime::from_unix_millis(2_000)));
    assert_eq!(s.ended_at, Some(SystemTime::from_unix_millis(9_000)));
    assert_eq!(running.ended_at, None, "a node still running has not ended");
}

/// **An endpoint node's summary names its provider, model and route** off its latest
/// `Spawned`, with the harness's spelling of the model undone (opencode's `marion/` prefix), and
/// a node with no provider carries none.
#[test]
fn a_summary_carries_an_endpoint_nodes_provider_model_and_route() {
    let spawned = |provider: Option<&str>| {
        line(
            1,
            2,
            RecordKind::Spawned(Spawned {
                agent_id: id("c"),
                harness_version: "1.17.3".into(),
                model: Some("marion/qwen/qwen3-coder".into()),
                pid: Some(3),
                start_id: None,
                provider: provider.map(str::to_string),
                route: provider.map(|_| "translated".to_string()),
                credential: provider.map(str::to_string),
            }),
        )
    };
    let intent_at = line(0, 1, intent("c", None, "opencode", 0));
    let s = summarize(
        &node_of(&[intent_at.clone(), spawned(Some("openrouter"))], "c"),
        false,
    )
    .unwrap();
    assert_eq!(
        s.endpoint,
        Some(marion_core::proto::NodeEndpoint {
            provider: "openrouter".into(),
            model: Some("qwen/qwen3-coder".into()),
            route: Some("translated".into()),
        })
    );
    assert_eq!(s.endpoint.unwrap().label(), "openrouter:qwen/qwen3-coder");
    let s = summarize(&node_of(&[intent_at, spawned(None)], "c"), false).unwrap();
    assert_eq!(s.endpoint, None);
}

/// **A recorded bound outranks the agent type's, because it is the one the node is under.**
///
/// The sibling above is the fallback; this is the normal case. `marion run --timeout 300` and
/// `spawn`'s `timeout_secs` both resolve a bound before the process exists, and the intent
/// records it — so the number `marion tree`'s detail pane prints is the clock the node is
/// actually being held to, and not its type's default wearing that clock's name.
#[test]
fn a_summary_reports_the_bound_the_launch_resolved() {
    let mut i = match intent("child", Some("root"), "codex-impl", 1) {
        RecordKind::SpawnIntent(i) => i,
        other => panic!("{other:?}"),
    };
    i.timeout_secs = Some(300);
    let n = node_of(&[line(0, 1, RecordKind::SpawnIntent(i))], "child");
    let s = summarize(&n, false).expect("a fully described node projects");
    assert_eq!(s.timeout, marion_core::encoding::Duration::from_secs(300));
    assert_ne!(
        s.timeout,
        agent_type::builtin("codex-impl").unwrap().timeout,
        "the type's default is what this node is *not* running under"
    );
}

/// **NC — a node marion cannot describe is refused by name, never summarized with invented
/// fields.**
///
/// Three ways, three sentences. The failure this rules out is the tempting one: default the
/// missing fields (`Harness::Codex`, depth 0, the 900 s bound) and hand back a summary that
/// reads exactly like a real node's. A client cannot tell those apart, which is the
/// partial-presented-as-complete shape this repo keeps refusing.
#[test]
fn an_undescribable_node_is_named_rather_than_filled_in() {
    // (1) records about a node, no identity for it.
    let orphan = node_of(
        &[line(
            0,
            1,
            RecordKind::Spawned(Spawned {
                agent_id: id("no-intent"),
                harness_version: "0.9.0".into(),
                model: None,
                pid: Some(3),
                start_id: None,
                provider: None,
                route: None,
                credential: None,
            }),
        )],
        "no-intent",
    );
    assert_eq!(summarize(&orphan, false), Err(Unprojectable::NoIntent));
    let e = Unprojectable::NoIntent.as_error(&id("no-intent"));
    assert_eq!(e.kind(), Some(FailureKind::Internal));
    assert!(e.message.contains("SpawnIntent"), "{e}");

    // (2) an agent type this build does not have — so §3.1's bound has no source.
    let unknown = node_of(&[line(0, 1, intent("a", None, "codex-turbo", 0))], "a");
    assert_eq!(
        summarize(&unknown, false),
        Err(Unprojectable::UnknownAgentType("codex-turbo".into()))
    );
    let e = Unprojectable::UnknownAgentType("codex-turbo".into()).as_error(&id("a"));
    assert_eq!(
        e.kind(),
        Some(FailureKind::NotFound),
        "`error.rs` names an agent type among the things NotFound is for"
    );
    assert!(e.message.contains("codex-turbo"), "{e}");

    // (3) a depth `NodeSummary`'s u8 cannot hold. §6.1's default max_depth is 3, so this is a
    // writer producing nonsense — and saturating it would place the node somewhere it is not.
    let deep = node_of(&[line(0, 1, intent("a", None, "codex-impl", 300))], "a");
    assert_eq!(
        summarize(&deep, false),
        Err(Unprojectable::DepthOutOfRange(300))
    );
    assert!(
        summarize(
            &node_of(&[line(0, 1, intent("a", None, "codex-impl", 255))], "a"),
            false
        )
        .is_ok(),
        "255 fits, so the boundary is the type's and not an arbitrary cap"
    );
}

/// **A user-defined type is projected from its recorded bound.** `summarize` runs under the
/// shared registry lock and reads no file, so it cannot resolve a `.marion/agents.toml` row —
/// but every production writer journals the bound the node launched under, and that is the
/// one thing the type was needed for. Only a journal that names no built-in *and* records no
/// bound is unprojectable: an intent written before the field existed, for a type this build
/// cannot look up.
#[test]
fn an_unknown_type_projects_from_its_recorded_bound_and_not_otherwise() {
    let intent_with = |bound: Option<u64>| {
        RecordKind::SpawnIntent(SpawnIntent {
            budget: None,
            review_of: None,
            agent_id: id("r"),
            parent_id: None,
            agent_type: "reviewer".into(),
            harness: Harness::Codex,
            depth: 1,
            task_id: None,
            timeout_secs: bound,
            verification: vec![],
            race: None,
            workflow: None,
        })
    };
    let bounded = node_of(&[line(0, 1, intent_with(Some(120)))], "r");
    let s = summarize(&bounded, false).expect("the recorded bound is enough");
    assert_eq!(s.agent_type, "reviewer");
    assert_eq!(s.harness, Harness::Codex);
    assert_eq!(s.timeout, marion_core::encoding::Duration::from_secs(120));
    let unbounded = node_of(&[line(0, 1, intent_with(None))], "r");
    assert_eq!(
        summarize(&unbounded, false),
        Err(Unprojectable::UnknownAgentType("reviewer".into()))
    );
}

/// **A resume re-resolves the recorded type from today's file, and refuses if the harness
/// moved.** The journal records the name and the harness; the file is the operator's and may
/// have been edited since. A row that now names another harness would relaunch a session that
/// harness has never seen under a node that claims to be the same one.
#[test]
fn a_recorded_type_is_resumed_only_under_the_harness_it_was_journaled_with() {
    let root = scratch("handler-recorded-type");
    let repo = marion_testsupport::fixture_repo(&root);
    let file = repo.join(crate::run::AGENT_TYPES_FILE);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let row = |harness: &str| {
        format!("[[agent]]\nname = \"reviewer\"\nharness = \"{harness}\"\ndescription = \"r\"\n")
    };
    let intent = SpawnIntent {
        budget: None,
        review_of: None,
        agent_id: id("r"),
        parent_id: None,
        agent_type: "reviewer".into(),
        harness: Harness::Codex,
        depth: 1,
        task_id: None,
        timeout_secs: Some(60),
        verification: vec![],
        race: None,
        workflow: None,
    };
    std::fs::write(&file, row("codex")).unwrap();
    assert_eq!(
        recorded_type(None, &repo, &intent).unwrap().harness,
        Harness::Codex
    );
    std::fs::write(&file, row("gemini")).unwrap();
    let e = recorded_type(None, &repo, &intent).unwrap_err();
    assert!(
        e.message.contains("journaled as codex") && e.message.contains("now says gemini"),
        "{e}"
    );
    std::fs::remove_file(&file).unwrap();
    let e = recorded_type(None, &repo, &intent).unwrap_err();
    assert_eq!(e.kind(), Some(FailureKind::NotFound), "{e}");
    // A built-in is resolved regardless of the file, and the file's own refusal is its own.
    let builtin = SpawnIntent {
        agent_type: "claude".into(),
        harness: Harness::ClaudeCode,
        ..intent.clone()
    };
    assert_eq!(
        recorded_type(None, &repo, &builtin).unwrap().harness,
        Harness::ClaudeCode
    );
    std::fs::write(&file, "[[agent]\n").unwrap();
    let e = recorded_type(None, &repo, &builtin).unwrap_err();
    assert!(e.message.contains("agents.toml"), "{e}");
}

/// Build a handle over a journal file, plus the recording sink a subscriber would be.
struct Fx {
    _dir: marion_testsupport::Scratch,
    path: std::path::PathBuf,
    handle: Arc<RegistryHandle>,
}

fn fx(tag: &str) -> Fx {
    fx_with(tag, vec![intent("root", None, "claude", 0)])
}

fn fx_with(tag: &str, records: Vec<RecordKind>) -> Fx {
    fx_with_runtime(tag, records, Arc::new(SystemQuitRuntime))
}

/// **The registry boots before the records are written, which is the production order.**
///
/// `marion run` starts the supervisor and *then* journals its root, so every node these tests
/// are about is a node the supervisor watched arrive. Writing the journal first and booting
/// over it is a different situation entirely — §7.2's restart, where a node already `Live` at
/// boot is one this supervisor has no record of deciding and is marked `Orphaned`
/// (`restart.rs`). A fixture in that shape would have every test below asserting over a tree of
/// orphans while claiming to describe a live fleet. `registry.rs` covers the restart order
/// directly.
fn fx_with_runtime(tag: &str, records: Vec<RecordKind>, runtime: Arc<dyn QuitRuntime>) -> Fx {
    let dir = scratch(tag);
    let path = dir.join("journal.jsonl");
    let live = Arc::new(LiveRegistry::follow(Registry::boot_path(&path).unwrap()));
    for (seq, kind) in records.into_iter().enumerate() {
        append(&path, &line(seq as u64, 1_000 + seq as u64, kind));
    }
    assert!(
        live.read(|r| r.restart_marks().is_empty()),
        "the supervisor booted over an empty journal; it lost nothing"
    );
    live.refresh();
    Fx {
        _dir: dir,
        path,
        handle: RegistryHandle::with_runtime(live, runtime),
    }
}

/// Records the process-tree operations selected by a disposition without asking the host
/// process table to cooperate. The real runtime delegates to `run.rs`; these tests are about
/// the handler's selection and ordering, so an injected observation makes a missed or extra
/// per-node operation an exact assertion rather than a timing-dependent survivor check.
#[derive(Default)]
struct RecordingRuntime {
    killed: Mutex<Vec<i32>>,
}

impl RecordingRuntime {
    fn killed(&self) -> Vec<i32> {
        lock(&self.killed).clone()
    }
}

impl QuitRuntime for RecordingRuntime {
    fn kill_process_tree_and_wait(&self, pid: i32) -> bool {
        lock(&self.killed).push(pid);
        true
    }
}

fn recording_fx_with(tag: &str, records: Vec<RecordKind>) -> (Fx, Arc<RecordingRuntime>) {
    let runtime = Arc::new(RecordingRuntime::default());
    let fx = fx_with_runtime(tag, records, runtime.clone());
    (fx, runtime)
}

fn spawned(agent: &str, pid: i32) -> RecordKind {
    RecordKind::Spawned(Spawned {
        agent_id: id(agent),
        harness_version: "test".into(),
        model: None,
        pid: Some(pid),
        start_id: None,
        provider: None,
        route: None,
        credential: None,
    })
}

fn state(agent: &str, state: NodeState) -> RecordKind {
    RecordKind::StateChanged(StateChanged {
        agent_id: id(agent),
        state,
        reason: None,
    })
}

fn quit(
    fx: &Fx,
    disposition: marion_core::proto::QuitDisposition,
) -> Result<marion_core::proto::result::SessionQuitResult, RpcError> {
    let out = crate::serve::sink(ConnId(9));
    fx.handle.hello_as_operator(ConnId(9));
    match fx.handle.call(
        ConnId(9),
        &Call::SessionQuit(marion_core::proto::params::SessionQuitParams { disposition }),
        &out,
    )? {
        MethodResult::SessionQuit(r) => Ok(r),
        other => panic!("wrong result: {}", other.method().as_str()),
    }
}

/// The file's records, decoded — the envelope, not just the payload. `journal_tags` answers
/// *what* was written; this answers *who wrote it and in what order*, which is a different
/// question and the one §4.2's `seq` and `mono_ns` are the answer to.
fn journal_records(path: &Path) -> Vec<JournalRecord> {
    std::fs::read(path)
        .unwrap()
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| marion_core::journal::decode(l).expect("a record marion wrote decodes"))
        .collect()
}

fn journal_tags(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            value["kind"]
                .as_object()
                .and_then(|o| o.keys().next())
                .cloned()
                .unwrap_or_else(|| value["kind"].as_str().unwrap_or("unknown").to_string())
        })
        .collect()
}

/// A pair of `Outbound`s is only obtainable from a live connection, so the subscription tests
/// run a real server over a real socket — which is also the only way to assert that the
/// notification reaches a *client* rather than a channel.
struct Wired {
    fx: Fx,
    server: Option<crate::serve::Server>,
    dir: std::path::PathBuf,
    sock: std::path::PathBuf,
}

impl Wired {
    fn new(tag: &str) -> Wired {
        let fx = fx(tag);
        let dir = std::path::PathBuf::from(format!("/tmp/mh-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = crate::socket::socket_paths(&dir, Path::new("/p"), 1);
        let crate::socket::Acquired::Serving(serving) = crate::socket::acquire(&paths).unwrap()
        else {
            panic!("nothing was listening")
        };
        let server = crate::serve::Server::start(
            serving,
            Arc::clone(&fx.handle) as Arc<dyn crate::serve::Handle>,
        );
        Wired {
            fx,
            server: Some(server),
            sock: paths.socket().to_path_buf(),
            dir,
        }
    }

    /// The operator's connection, past its `session/hello`.
    fn dial(&self) -> std::os::unix::net::UnixStream {
        let mut s = std::os::unix::net::UnixStream::connect(&self.sock).expect("dial");
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let who = crate::client_auth::Identity::Operator(
            self.fx.handle.operator_hello().operator.expect("a key"),
        );
        crate::client_auth::hello(&mut s, &who).expect("the operator's hello");
        s
    }
}

impl Drop for Wired {
    fn drop(&mut self) {
        if let Some(s) = self.server.take() {
            s.stop();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn call(s: &mut std::os::unix::net::UnixStream, call: Call, id: i64) {
    let f = Frame::Request(marion_core::proto::Request::new(
        marion_core::proto::RequestId::Number(id),
        call,
    ));
    s.write_all(f.to_line().as_bytes()).unwrap();
    s.flush().unwrap();
}

fn next_frame(r: &mut std::io::BufReader<std::os::unix::net::UnixStream>) -> Frame {
    use std::io::BufRead;
    let mut line = String::new();
    assert!(r.read_line(&mut line).unwrap() > 0, "the socket closed");
    Frame::from_line(&line).expect("well-formed")
}

/// `node/get` over the socket, against a tree that came out of a journal — the request/response
/// shape, end to end.
#[test]
fn node_get_answers_from_the_journal_and_refuses_a_node_it_has_no_record_of() {
    let w = Wired::new("handler-get");
    let mut c = w.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());

    call(
        &mut c,
        Call::NodeGet(marion_core::proto::params::NodeGetParams::of(id("root"))),
        1,
    );
    let Frame::Response(resp) = next_frame(&mut r) else {
        panic!("expected a response")
    };
    let marion_core::proto::Outcome::Result(body) = resp.outcome else {
        panic!("expected a result")
    };
    let MethodResult::NodeGet(got) = marion_core::proto::Method::NodeGet
        .decode_result(&body)
        .unwrap()
    else {
        panic!("wrong result type")
    };
    assert_eq!(got.node.agent_id, id("root"));
    assert_eq!(got.node.harness, Harness::ClaudeCode);
    assert_eq!(got.node.depth, 0);

    // A node the journal does not record is a refusal that says how much has been read, so an
    // operator can tell "no such node" from "not yet".
    call(
        &mut c,
        Call::NodeGet(marion_core::proto::params::NodeGetParams::of(id("nobody"))),
        2,
    );
    let Frame::Response(resp) = next_frame(&mut r) else {
        panic!("expected a response")
    };
    let marion_core::proto::Outcome::Error(e) = resp.outcome else {
        panic!("expected a refusal")
    };
    assert_eq!(e.kind(), Some(FailureKind::NotFound));
    assert!(e.is_refusal(), "a missing node is the caller's business");
    assert!(e.message.contains("records read"), "{e}");
}

/// **The subscription shape, against the real journal**: a snapshot, then a notification for a
/// node that appeared afterwards — written by a *second* writer, which is the case the registry
/// tails the file for in the first place.
#[test]
fn tree_subscribe_returns_a_snapshot_and_then_narrates_what_the_journal_says_next() {
    let w = Wired::new("handler-sub");
    let mut c = w.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());

    call(
        &mut c,
        Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
        1,
    );
    let Frame::Response(resp) = next_frame(&mut r) else {
        panic!("expected a response")
    };
    let marion_core::proto::Outcome::Result(body) = resp.outcome else {
        panic!("expected a result")
    };
    let MethodResult::TreeSubscribe(snap) = marion_core::proto::Method::TreeSubscribe
        .decode_result(&body)
        .unwrap()
    else {
        panic!("wrong result type")
    };
    assert_eq!(snap.nodes.len(), 1, "the root the journal already had");
    assert_eq!(snap.nodes[0].agent_id, id("root"));
    assert_eq!(snap.read_point.records, 1);

    // A different process appends a child and then moves it.
    append(
        &w.fx.path,
        &line(1, 2_000, intent("child", Some("root"), "codex-impl", 1)),
    );
    let Frame::Notification(n) = next_frame(&mut r) else {
        panic!("a node that appeared must arrive as tree/node-added")
    };
    let Event::NodeAdded { node, ts } = n.event else {
        panic!("expected tree/node-added, got {:?}", n.event.method())
    };
    assert_eq!(node.agent_id, id("child"));
    assert_eq!(node.parent_id, Some(id("root")));
    assert_eq!(
        ts,
        SystemTime::from_unix_millis(2_000),
        "the journal's time, not the follower's — a client renders this as when it happened"
    );

    append(
        &w.fx.path,
        &line(
            2,
            3_500,
            RecordKind::StateChanged(StateChanged {
                agent_id: id("child"),
                state: NodeState::Running,
                reason: None,
            }),
        ),
    );
    let Frame::Notification(n) = next_frame(&mut r) else {
        panic!("expected a notification")
    };
    let Event::NodeState {
        agent_id,
        state,
        reap_state,
        ts,
    } = n.event
    else {
        panic!("expected node/state")
    };
    assert_eq!(agent_id, id("child"));
    assert_eq!(state, NodeState::Running);
    assert_eq!(
        reap_state,
        ReapState::Live,
        "§7.6 gates on the disjunction, so the two travel in one message"
    );
    assert_eq!(ts, SystemTime::from_unix_millis(3_500));

    // The child's sink meters what its stream says it spent and publishes it to this owner's
    // live figures as the frames are recorded: the row's total arrives as the whole summary
    // again, the one notification an older client already folds as a replacement.
    let sink = crate::events::EventSink::new(
        crate::events::EventWriter::open_path(
            &w.fx.path.with_file_name("child.jsonl"),
            &id("child"),
        )
        .unwrap(),
        Harness::Codex,
        "unused".into(),
    )
    .publishing_to(&id("child"), Some(w.fx.handle.spending.clone()));
    for l in marion_testsupport::app_server_capture("p4-items.jsonl").lines() {
        sink.record_line(l);
    }
    let spent = sink
        .spent()
        .usage
        .expect("the fixture states its usage")
        .total();
    // One replacement per total that moved — codex states its thread's running total after
    // every response — and the last is the whole spend.
    let node = loop {
        let Frame::Notification(n) = next_frame(&mut r) else {
            panic!("expected a notification")
        };
        let Event::NodeAdded { node, .. } = n.event else {
            panic!(
                "a total that moved must arrive as tree/node-added, got {:?}",
                n.event.method()
            )
        };
        assert_eq!(node.agent_id, id("child"));
        assert!(
            node.tokens.is_some_and(|t| t <= spent),
            "a running total never passes the whole: {:?} of {spent}",
            node.tokens
        );
        if node.tokens == Some(spent) {
            break node;
        }
    };
    assert_eq!(
        node.state,
        NodeState::Running,
        "the replacement is the whole, current row"
    );
}

/// **NC — a node the snapshot could not describe is excluded *and counted*, never silently
/// dropped.**
///
/// `TreeSubscribeResult` has nowhere to say "and there are N I could not describe", so the count
/// lives on the supervisor. The assertion is that it is not zero: a gap that is admitted is a
/// different thing from a gap that is invisible, and the invisible version is the one §11 item
/// 23 keeps naming.
#[test]
fn a_node_the_snapshot_cannot_describe_is_counted_rather_than_dropped_into_silence() {
    let w = Wired::new("handler-lost");
    // A node with records and no identity, written by a second writer.
    append(
        &w.fx.path,
        &line(
            1,
            2_000,
            RecordKind::Spawned(Spawned {
                agent_id: id("headless"),
                harness_version: "0.9.0".into(),
                model: None,
                pid: Some(9),
                start_id: None,
                provider: None,
                route: None,
                credential: None,
            }),
        ),
    );
    assert!(until(
        || w.fx.handle.live.read(|r| r.tree().nodes().len()) == 2
    ));

    let mut c = w.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    call(
        &mut c,
        Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
        1,
    );
    let Frame::Response(resp) = next_frame(&mut r) else {
        panic!("expected a response")
    };
    let marion_core::proto::Outcome::Result(body) = resp.outcome else {
        panic!("expected a result")
    };
    let MethodResult::TreeSubscribe(snap) = marion_core::proto::Method::TreeSubscribe
        .decode_result(&body)
        .unwrap()
    else {
        panic!("wrong result type")
    };
    assert_eq!(
        snap.nodes
            .iter()
            .map(|n| n.agent_id.clone())
            .collect::<Vec<_>>(),
        [id("root")],
        "a node with no identity is not described"
    );
    assert_eq!(
        w.fx.handle.unprojectable(),
        1,
        "and it is not invisible either"
    );
}

/// A subscriber that goes away stops being one, so a supervisor with no clients holds no queues
/// — and, per §7.3.1, nothing else happens at all.
#[test]
fn a_departed_client_stops_being_a_subscriber_and_nothing_else_changes() {
    let w = Wired::new("handler-gone");
    let before = w.fx.handle.live.read(|r| r.tree().nodes().len());
    {
        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        call(
            &mut c,
            Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
            1,
        );
        next_frame(&mut r);
        assert_eq!(w.fx.handle.subscribers(), 1);
    }
    assert!(
        until(|| w.fx.handle.subscribers() == 0),
        "a closed connection is not a subscriber"
    );
    assert_eq!(
        w.fx.handle.live.read(|r| r.tree().nodes().len()),
        before,
        "§7.3.1: from the registry's point of view nothing happened"
    );
}

/// **NC — disposition (b) changes no node and cannot end the supervisor.**
///
/// The journal bytes are the authority on the first half: comparing a summary before and after
/// could miss an appended record that happens not to project into state. The second call on the
/// same socket is the authority on the second half: a `Resident` word in a response is not proof
/// that the server actually remained resident.
#[test]
fn detach_all_leaves_the_journal_and_node_untouched_and_the_supervisor_serving() {
    let fx = fx("handler-quit-detach");
    let before = std::fs::read(&fx.path).unwrap();
    let state = fx
        .handle
        .live
        .read(|r| r.tree().get(&id("root")).unwrap().state);
    let out = crate::serve::sink(ConnId(1));

    fx.handle.hello_as_operator(ConnId(1));
    let result = fx
        .handle
        .call(
            ConnId(1),
            &Call::SessionQuit(marion_core::proto::params::SessionQuitParams {
                disposition: marion_core::proto::QuitDisposition::DetachAll,
            }),
            &out,
        )
        .expect("detach is implemented, not refused");
    let MethodResult::SessionQuit(result) = result else {
        panic!("wrong result type")
    };
    let marion_core::proto::QuitOutcome::Detached {
        detached,
        gate_exposed,
        guidance,
        supervisor,
    } = result.outcome
    else {
        panic!("detach returned another disposition's outcome")
    };
    assert_eq!(detached, [id("root")]);
    assert_eq!(gate_exposed, [id("root")]);
    assert!(guidance.reattach.contains("tree/subscribe"));
    assert!(guidance.stop_fleet.contains("session/quit"));
    assert!(guidance.reattach.contains("nobody can approve"));
    assert!(guidance.stop_fleet.contains("marion cancel"));
    assert!(!guidance.reattach.contains('§'));
    assert_eq!(
        supervisor,
        marion_core::proto::SupervisorDisposition::Resident(
            marion_core::proto::ResidentReason::SpawnOutstanding
        )
    );
    assert_eq!(std::fs::read(&fx.path).unwrap(), before);
    assert_eq!(
        fx.handle
            .live
            .read(|tree| tree.tree().get(&id("root")).unwrap().state),
        state
    );

    fx.handle.hello_as_operator(ConnId(1));
    assert!(matches!(
        fx.handle.call(
            ConnId(1),
            &Call::NodeGet(marion_core::proto::params::NodeGetParams::of(id("root"))),
            &out,
        ),
        Ok(MethodResult::NodeGet(_))
    ));
    assert!(!fx.handle.exiting());
    assert!(!fx.handle.idle_exit_eligible());
}

/// **NC — disposition (a) is unreachable when the confirmation is not the live render.**
///
/// The empty runtime trace is the negative control: checking only for a refusal could pass
/// after a buggy implementation signalled first and noticed the mismatch second. It and the
/// byte-identical journal prove the refusal preceded every side effect.
#[test]
fn kill_tree_refuses_a_missing_or_stale_confirmed_list_before_signalling_anything() {
    let (fx, runtime) = recording_fx_with(
        "handler-quit-kill-unconfirmed",
        vec![
            intent("root", None, "claude", 0),
            spawned("root", 101),
            state("root", NodeState::Running),
        ],
    );
    let before = std::fs::read(&fx.path).unwrap();

    let error = quit(
        &fx,
        marion_core::proto::QuitDisposition::KillTree { confirmed: vec![] },
    )
    .expect_err("an empty render did not confirm a live root");
    assert_eq!(error.kind(), Some(FailureKind::Refused));
    assert!(error.message.contains("confirmed"), "{error}");
    assert!(
        runtime.killed().is_empty(),
        "the mismatch was checked after signalling"
    );
    assert_eq!(std::fs::read(&fx.path).unwrap(), before);
}

/// Disposition (a), positively: every live node is killed through its own recorded PID, its
/// prior activity is returned, each intent precedes its confirmation, and only then is the
/// supervisor exit recorded. Two distinct recorded PIDs make a one-node or one-group
/// implementation visible in the runtime trace.
#[test]
fn kill_tree_kills_each_non_terminal_node_and_journals_each_pair_before_exit() {
    let (fx, runtime) = recording_fx_with(
        "handler-quit-kill",
        vec![
            intent("root", None, "claude", 0),
            spawned("root", 101),
            state("root", NodeState::Running),
            intent("child", Some("root"), "codex-impl", 1),
            spawned("child", 202),
            state("child", NodeState::Blocked(BlockReason::Permission)),
            intent("done", Some("root"), "codex-impl", 1),
            RecordKind::Exited(Exited {
                agent_id: id("done"),
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "already done".into(),
                },
            }),
        ],
    );

    let disposition = marion_core::proto::QuitDisposition::KillTree {
        confirmed: vec![id("child"), id("root")],
    };
    fx.handle.connected(ConnId(9));
    let result = quit(&fx, disposition.clone()).expect("the exact set was confirmed");
    let marion_core::proto::QuitOutcome::Killed { nodes, supervisor } = result.outcome else {
        panic!("kill returned another disposition's outcome")
    };
    assert_eq!(
        nodes,
        [
            marion_core::proto::KilledNode {
                agent_id: id("root"),
                was: NodeState::Running,
            },
            marion_core::proto::KilledNode {
                agent_id: id("child"),
                was: NodeState::Blocked(BlockReason::Permission),
            },
        ]
    );
    assert_eq!(
        supervisor,
        marion_core::proto::SupervisorDisposition::Exiting
    );
    assert_eq!(runtime.killed(), [101, 202], "one operation per live node");
    let mut tags = journal_tags(&fx.path);
    assert_eq!(
        &tags[8..],
        ["KillIntent", "KillConfirmed", "KillIntent", "KillConfirmed",],
        "one intent/act/confirm pair per node"
    );
    assert!(
        !fx.handle.begin_idle_exit(),
        "§5.7 forbids the exit record while even the quitting client remains"
    );
    fx.handle.gone(
        ConnId(9),
        &ClientGone::Quit(disposition),
        &Departure::QuitCompleted,
    );
    assert!(
        fx.handle.begin_idle_exit(),
        "after the configured grace, zero clients and zero non-terminals permit exit"
    );
    tags = journal_tags(&fx.path);
    assert_eq!(tags.last().unwrap(), "SupervisorExited");
    assert!(fx.handle.exiting());
    let replay = crate::journal::read_path(&fx.path).unwrap();
    assert_eq!(
        replay.get(&id("root")).unwrap().state,
        NodeState::Exited(ExitStatus::Cancelled)
    );
    assert_eq!(
        replay.get(&id("child")).unwrap().state,
        NodeState::Exited(ExitStatus::Cancelled)
    );
    assert_eq!(
        replay.get(&id("done")).unwrap().state,
        NodeState::Exited(ExitStatus::Ok),
        "quitting does not rewrite an existing terminal"
    );
}

/// **Disposition (a) reaches a real process and leaves it dead** — the other half of §11 item
/// 28 step 1, and the half no other test in this file can make.
///
/// Every other `KillTree` test injects [`RecordingRuntime`], which records a pid and signals
/// nothing. That is deliberate — they are about *selection and ordering* — but it means the
/// whole set could stay green over a `kill_process_tree_and_wait` that did nothing at all. This
/// one runs the **real** [`SystemQuitRuntime`] against a **real** process started in its own
/// process group, which is what a child marion spawns is (`run_bounded_with`'s
/// `process_group(0)`), and asserts the process is gone afterwards.
///
/// It is only reachable because `Spawned` now carries a pid. Until item 28 step 1 every
/// production writer recorded `pid: None`, the preflight below refused whenever there was
/// anything to kill, and this disposition could not fire on a real fleet at all — a
/// mutation-audited path with no production path to it. The refusal itself is pinned
/// separately, over the record shape rather than over a live process, by
/// `kill_tree_refuses_a_confirmed_node_whose_pid_is_not_recorded_yet`.
///
/// **Two negative controls, because "everything is already dead" passes vacuously.** The
/// confirmed set is asserted non-empty before the call, and the process is asserted *alive*
/// before it — a `kill_tree` over an all-terminal tree signals nothing and succeeds, and would
/// satisfy every other assertion here.
///
/// Liveness is read three-valued through `ps`: the test is the killed process's parent, so
/// between the signal and the reap it is a **zombie**, which `kill(pid, 0)` reports as alive
/// and which `kill_process_tree_and_wait` correctly counts as dead.
#[test]
fn kill_tree_over_the_real_runtime_leaves_the_recorded_process_dead() {
    use marion_testsupport::{Liveness, liveness};
    use std::os::unix::process::CommandExt;

    // Its own group, so the group-addressed kill reaches it and cannot reach the test runner:
    // `signal_targets` refuses marion's own pgid, so without this the signal lands nowhere.
    let mut victim = std::process::Command::new("sleep")
        .arg("600")
        .process_group(0)
        .spawn()
        .expect("a `sleep` starts");
    let pid = victim.id() as i32;
    assert_eq!(
        liveness(pid),
        Liveness::Alive,
        "NC: the victim must be alive before the kill, or every assertion below is vacuous"
    );

    let fx = fx_with(
        "handler-quit-kill-real",
        vec![
            intent("root", None, "claude", 0),
            spawned("root", pid),
            state("root", NodeState::Running),
        ],
    );
    let confirmed = vec![id("root")];
    assert!(
        !confirmed.is_empty(),
        "NC: an empty confirmed set means an all-terminal tree, which this disposition \
         satisfies without signalling anything"
    );

    let result = quit(
        &fx,
        marion_core::proto::QuitDisposition::KillTree { confirmed },
    );
    // Reap before asserting, unconditionally: a failure here must not also leak the victim.
    let outcome = result.map(|r| r.outcome);
    let after = liveness(pid);
    let _ = victim.kill();
    let _ = victim.wait();

    let marion_core::proto::QuitOutcome::Killed { nodes, .. } =
        outcome.expect("the exact live set was confirmed")
    else {
        panic!("kill returned another disposition's outcome")
    };
    assert_eq!(nodes.len(), 1, "one live node, one kill: {nodes:?}");
    assert_ne!(
        after,
        Liveness::Alive,
        "the journal named pid {pid}, marion said it killed it, and it is still running"
    );
    assert_ne!(
        after,
        Liveness::CannotTell,
        "`ps` could not be asked, so nothing here was measured"
    );
    assert_eq!(
        liveness(pid),
        Liveness::Gone,
        "and once reaped it is absent outright"
    );
}

/// **NC — disposition (c) applies §7.2's predicate, not a convenient approximation.**
///
/// Idle processes die and get exactly one reap pair. Running, every `Blocked(_)`, and a
/// spawning node survive untouched and are returned with detach guidance; an already-terminal
/// node appears in neither list. The exact runtime trace is the mutation control against an
/// implementation that simply sweeps every PID it can see.
#[test]
fn reap_idle_detach_busy_reaps_only_idle_and_detaches_every_refusal_class() {
    let (fx, runtime) = recording_fx_with(
        "handler-quit-reap",
        vec![
            intent("idle", None, "claude", 0),
            spawned("idle", 11),
            state("idle", NodeState::Idle),
            intent("running", Some("idle"), "codex-impl", 1),
            spawned("running", 22),
            state("running", NodeState::Running),
            intent("permission", Some("idle"), "codex-impl", 1),
            spawned("permission", 33),
            state("permission", NodeState::Blocked(BlockReason::Permission)),
            intent("elicitation", Some("idle"), "codex-impl", 1),
            spawned("elicitation", 44),
            state("elicitation", NodeState::Blocked(BlockReason::Elicitation)),
            intent("descendants", Some("idle"), "codex-impl", 1),
            spawned("descendants", 55),
            state("descendants", NodeState::Blocked(BlockReason::Descendants)),
            intent("waiting-parent", None, "claude", 0),
            spawned("waiting-parent", 66),
            state(
                "waiting-parent",
                NodeState::Blocked(BlockReason::Descendants),
            ),
            intent("idle-spawn-target", Some("waiting-parent"), "codex-impl", 1),
            spawned("idle-spawn-target", 77),
            state("idle-spawn-target", NodeState::Idle),
            intent("spawning", Some("idle"), "codex-impl", 1),
            intent("done", Some("idle"), "codex-impl", 1),
            RecordKind::Exited(Exited {
                agent_id: id("done"),
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "already done".into(),
                },
            }),
        ],
    );

    let result = quit(&fx, marion_core::proto::QuitDisposition::ReapIdleDetachBusy)
        .expect("the default disposition is implemented");
    let marion_core::proto::QuitOutcome::ReapedAndDetached {
        reaped,
        detached,
        gate_exposed,
        guidance,
        supervisor,
    } = result.outcome
    else {
        panic!("reap returned another disposition's outcome")
    };
    assert_eq!(reaped, [id("idle")]);
    assert_eq!(
        detached,
        [
            id("running"),
            id("permission"),
            id("elicitation"),
            id("descendants"),
            id("waiting-parent"),
            id("idle-spawn-target"),
            id("spawning"),
        ]
    );
    assert_eq!(gate_exposed, detached);
    assert!(guidance.reattach.contains("tree/subscribe"));
    assert!(guidance.reattach.contains("reaped"));
    assert!(guidance.reattach.contains("resumable"));
    assert!(guidance.stop_fleet.contains("session/quit"));
    assert_eq!(
        supervisor,
        marion_core::proto::SupervisorDisposition::Resident(
            marion_core::proto::ResidentReason::BlockedNode
        )
    );
    assert!(!fx.handle.idle_exit_eligible());
    assert_eq!(runtime.killed(), [11], "§7.2 refusal classes were detached");
    let tags = journal_tags(&fx.path);
    assert_eq!(&tags[24..], ["ReapIntent", "ReapConfirmed"]);
    let replay = crate::journal::read_path(&fx.path).unwrap();
    assert_eq!(
        replay.get(&id("idle")).unwrap().reap_state,
        ReapState::ReapedIdle
    );
    for agent in [
        "running",
        "permission",
        "elicitation",
        "descendants",
        "waiting-parent",
        "idle-spawn-target",
        "spawning",
    ] {
        let node = replay.get(&id(agent)).unwrap();
        assert_eq!(node.reap_state, ReapState::Live, "{agent}");
        assert!(node.reap_intent.is_none(), "{agent}");
    }
}

/// **NC — a spawn marion *abandoned* is not a spawn that is *outstanding*.**
///
/// The node's state is `Spawning` and stays `Spawning` forever, because `registry.rs` records
/// the abort as a separate fact rather than as a transition — so on the unfiltered reading this
/// one node satisfies two of §5.7's clauses at once and the supervisor can never exit.
///
/// That is not a hypothetical. A `marion run` whose root failed to launch journals exactly
/// these two records (`root.rs`'s `Err` arm), and with the detached supervisor wired up it left
/// a process resident over a project directory the run had already deleted. §7.2's rule is the
/// argument in one line: *"a node marion decided the fate of is never `Orphaned`"* — its fate
/// is decided, there is no process, and there is nothing for exiting to strand.
///
/// The second half is what stops the fix from being a blanket "ignore `Spawning`": an intent
/// with **no** abort beside it still holds the supervisor, because that one really is
/// outstanding.
#[test]
fn an_aborted_spawn_does_not_keep_the_supervisor_resident_but_an_outstanding_one_does() {
    let aborted = fx_with(
        "handler-quit-spawn-aborted",
        vec![
            intent("root", None, "claude", 0),
            RecordKind::SpawnAborted(marion_core::journal::SpawnAborted {
                agent_id: id("root"),
                reason: "the harness binary was not found".into(),
            }),
        ],
    );
    let marion_core::proto::QuitOutcome::Detached {
        supervisor,
        detached,
        ..
    } = quit(&aborted, marion_core::proto::QuitDisposition::DetachAll)
        .unwrap()
        .outcome
    else {
        panic!("DetachAll answers Detached")
    };
    assert_eq!(
        supervisor,
        marion_core::proto::SupervisorDisposition::Exiting,
        "an abandoned spawn strands nothing, so §5.7's exclusion list does not name it"
    );
    // Measured 2026-09-30: `marion run` printed "still running in the background: <id>" for a
    // child whose launch failed before any process existed, beside "no node holds this supervisor".
    assert_eq!(
        detached,
        Vec::<AgentId>::new(),
        "and it is not reported as a node left running in the background"
    );
    assert!(
        aborted.handle.idle_exit_eligible(),
        "and the accept loop may act on that"
    );

    let outstanding = fx_with(
        "handler-quit-spawn-outstanding",
        vec![intent("root", None, "claude", 0)],
    );
    let marion_core::proto::QuitOutcome::Detached { supervisor, .. } =
        quit(&outstanding, marion_core::proto::QuitDisposition::DetachAll)
            .unwrap()
            .outcome
    else {
        panic!("DetachAll answers Detached")
    };
    assert_eq!(
        supervisor,
        marion_core::proto::SupervisorDisposition::Resident(
            marion_core::proto::ResidentReason::SpawnOutstanding
        ),
        "an intent with no resolution beside it is exactly what §5.7 means by outstanding"
    );
    assert!(!outstanding.handle.idle_exit_eligible());
}

/// **NC — an abort written *after* the process existed is not evidence that it does not.**
///
/// The narrow reading above — *no `Spawned`, no pid, so nothing to strand* — is the whole of
/// what an abandoned spawn licenses. `run.rs`'s `AbortOnDrop` stays armed across the entire
/// synchronous child run, and a `Child` that is dropped rather than reaped does **not** kill
/// the process it holds, so a panic anywhere between `command.spawn()` and the disarm writes
/// `SpawnAborted` beside a `Spawned` that names a live pid. Discarding that node would let the
/// supervisor exit over a process it can name.
#[test]
fn an_abort_written_after_the_child_was_spawned_still_holds_the_supervisor() {
    let fx = fx_with(
        "handler-quit-abort-after-spawn",
        vec![
            intent("root", None, "claude", 0),
            spawned("root", 4242),
            RecordKind::SpawnAborted(marion_core::journal::SpawnAborted {
                agent_id: id("root"),
                reason: "marion left the spawn path before the child reached a terminal record"
                    .into(),
            }),
        ],
    );
    let marion_core::proto::QuitOutcome::Detached { supervisor, .. } =
        quit(&fx, marion_core::proto::QuitDisposition::DetachAll)
            .unwrap()
            .outcome
    else {
        panic!("DetachAll answers Detached")
    };
    assert_eq!(
        supervisor,
        marion_core::proto::SupervisorDisposition::Resident(
            marion_core::proto::ResidentReason::SpawnOutstanding
        ),
        "the journal names pid 4242 and never says it died; exiting here strands it"
    );
    assert!(
        !fx.handle.idle_exit_eligible(),
        "and the accept loop must not act on the discarded reading either"
    );
}

/// **NC — a registry that stopped following says *that*, not whichever stale clause the frozen
/// prefix happens to satisfy.**
///
/// Every node here is terminal, so the honest answer to §5.7 is `Exiting` and the *only* thing
/// keeping this supervisor is that it can no longer read the file it would answer from
/// (§7.4). Failing closed is right and is unchanged. What is asserted is the sentence: an
/// operator told `NonTerminalNode` goes looking for a node that finished, while the fact is
/// that marion stopped reading at a byte — and only one of those two is actionable.
#[test]
fn a_registry_that_stopped_following_is_reported_as_that_and_not_as_a_stale_node() {
    let fx = fx_with(
        "handler-quit-registry-stopped",
        vec![
            intent("root", None, "claude", 0),
            spawned("root", 12),
            RecordKind::Exited(Exited {
                agent_id: id("root"),
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "finished".into(),
                },
            }),
        ],
    );
    let marion_core::proto::QuitOutcome::Detached { supervisor, .. } =
        quit(&fx, marion_core::proto::QuitDisposition::DetachAll)
            .unwrap()
            .outcome
    else {
        panic!("DetachAll answers Detached")
    };
    assert_eq!(
        supervisor,
        marion_core::proto::SupervisorDisposition::Exiting,
        "the control: with the journal readable, nothing here holds it"
    );

    append(&fx.path, b"this is a complete line and not a record\n");
    let marion_core::proto::QuitOutcome::Detached { supervisor, .. } =
        quit(&fx, marion_core::proto::QuitDisposition::DetachAll)
            .unwrap()
            .outcome
    else {
        panic!("DetachAll answers Detached")
    };
    assert_eq!(
        supervisor,
        marion_core::proto::SupervisorDisposition::Resident(
            marion_core::proto::ResidentReason::RegistryStopped
        ),
        "§7.4: the tree is frozen, so no clause read off it may be quoted as the reason"
    );
    assert!(
        !fx.handle.idle_exit_eligible(),
        "and the accept loop fails closed on the same reading"
    );
}

/// **NC — a detach that found work still arms the exit that the work later releases.**
///
/// §5.7's predicate is evaluated by the accept loop, continuously, not once at the instant a
/// client asked. A quit whose answer is `Resident` is not a quit that failed: the client still
/// left, and the clause that held the supervisor can clear a millisecond later. If the answer
/// at that one instant decided whether the timer may ever start, a fleet that finishes just
/// after the last window closes keeps a supervisor forever.
#[test]
fn a_detach_that_found_work_still_arms_the_exit_that_work_later_releases() {
    let fx = fx_with(
        "handler-quit-resident-then-released",
        vec![
            intent("root", None, "claude", 0),
            spawned("root", 77),
            state("root", NodeState::Running),
        ],
    );
    fx.handle.connected(ConnId(9));
    let marion_core::proto::QuitOutcome::Detached { supervisor, .. } =
        quit(&fx, marion_core::proto::QuitDisposition::DetachAll)
            .unwrap()
            .outcome
    else {
        panic!("DetachAll answers Detached")
    };
    assert_eq!(
        supervisor,
        marion_core::proto::SupervisorDisposition::Resident(
            marion_core::proto::ResidentReason::NonTerminalNode
        )
    );
    fx.handle.gone(
        ConnId(9),
        &ClientGone::Quit(marion_core::proto::QuitDisposition::DetachAll),
        &Departure::QuitCompleted,
    );
    assert!(
        !fx.handle.idle_exit_eligible(),
        "while the node runs, the node is the answer"
    );

    append(
        &fx.path,
        &line(
            3,
            1_003,
            RecordKind::Exited(Exited {
                agent_id: id("root"),
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "the node finished a moment after the window closed".into(),
                },
            }),
        ),
    );
    assert!(
        fx.handle.idle_exit_eligible(),
        "nothing in §5.7's exclusion list holds any more, and no second client is coming to \
         ask again"
    );
}

/// **NC — §5.7's exit predicate is zero clients and zero non-terminal nodes, and nothing else.**
///
/// A dropped socket is not a quit (§2, §7.3.1) and this changes nothing about that: no node is
/// touched, nothing is journaled about the departure, and every clause of the exclusion list
/// still decides the answer. What it must not do is make the *supervisor's own* lifetime
/// conditional on a client having been polite — a TUI that was SIGKILLed leaves a supervisor
/// with nothing to supervise, and §5.7 says that supervisor MAY go.
#[test]
fn a_client_that_vanished_without_quitting_still_leaves_an_empty_supervisor_free_to_exit() {
    let fx = fx_with(
        "handler-exit-after-socket-closed",
        vec![
            intent("root", None, "claude", 0),
            spawned("root", 55),
            RecordKind::Exited(Exited {
                agent_id: id("root"),
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "the run finished".into(),
                },
            }),
        ],
    );
    fx.handle.connected(ConnId(9));
    assert!(
        !fx.handle.idle_exit_eligible(),
        "a client is attached, which is §5.7's absolute clause"
    );
    fx.handle
        .gone(ConnId(9), &ClientGone::SocketClosed, &Departure::Eof);
    assert!(
        fx.handle.idle_exit_eligible(),
        "zero clients and zero non-terminal nodes is the whole predicate (§5.7)"
    );
    assert_eq!(
        journal_tags(&fx.path),
        ["SpawnIntent", "Spawned", "Exited"],
        "§7.3.1: nothing is journaled about a client's death"
    );
}

/// `ReapedIdle` is resumable and therefore not `Exited(_)`. §5.7's zero-non-terminal rule is
/// literal: reaping the last process does not permit the supervisor to journal an exit while
/// that resumable node remains in the registry.
#[test]
fn a_reaped_idle_node_still_keeps_the_supervisor_resident() {
    let (fx, runtime) = recording_fx_with(
        "handler-quit-reaped-resident",
        vec![
            intent("idle", None, "claude", 0),
            spawned("idle", 88),
            state("idle", NodeState::Idle),
        ],
    );
    let outcome = quit(&fx, marion_core::proto::QuitDisposition::ReapIdleDetachBusy)
        .unwrap()
        .outcome;
    let marion_core::proto::QuitOutcome::ReapedAndDetached { supervisor, .. } = outcome else {
        panic!("wrong disposition outcome")
    };
    assert_eq!(
        supervisor,
        marion_core::proto::SupervisorDisposition::Resident(
            marion_core::proto::ResidentReason::NonTerminalNode
        )
    );
    assert_eq!(runtime.killed(), [88]);
    assert!(!fx.handle.idle_exit_eligible());
    assert!(
        journal_tags(&fx.path)
            .iter()
            .all(|tag| tag != "SupervisorExited")
    );
}

#[test]
fn an_unconfirmed_reap_intent_forbids_the_supervisor_exit_record() {
    let fx = fx_with(
        "handler-quit-unconfirmed-reap",
        vec![
            intent("idle", None, "claude", 0),
            state("idle", NodeState::Idle),
            RecordKind::ReapIntent(ReapIntent {
                agent_id: id("idle"),
                reason: "a prior supervisor decided to reap".into(),
            }),
        ],
    );
    let outcome = quit(&fx, marion_core::proto::QuitDisposition::DetachAll)
        .unwrap()
        .outcome;
    let marion_core::proto::QuitOutcome::Detached { supervisor, .. } = outcome else {
        panic!("wrong disposition outcome")
    };
    assert_eq!(
        supervisor,
        marion_core::proto::SupervisorDisposition::Resident(
            marion_core::proto::ResidentReason::UnconfirmedReapIntent
        )
    );
    assert!(!fx.handle.idle_exit_eligible());
    assert!(
        journal_tags(&fx.path)
            .iter()
            .all(|tag| tag != "SupervisorExited")
    );
}

/// **The §7.2 restart order**, which every other fixture here deliberately inverts: the journal
/// is written and *then* the supervisor boots over it, so its nodes are marked `Orphaned`.
fn restart_fx_with(tag: &str, records: Vec<RecordKind>) -> Fx {
    let dir = scratch(tag);
    let path = dir.join("journal.jsonl");
    for (seq, kind) in records.into_iter().enumerate() {
        append(&path, &line(seq as u64, 1_000 + seq as u64, kind));
    }
    let live = Arc::new(LiveRegistry::follow(Registry::boot_path(&path).unwrap()));
    assert!(
        !live.read(|r| r.restart_marks().is_empty()),
        "the point of this fixture is a boot that marked something",
    );
    Fx {
        _dir: dir,
        path,
        handle: RegistryHandle::with_runtime(live, Arc::new(SystemQuitRuntime)),
    }
}

/// **An `Orphaned` node holds the supervisor resident**, which is the clause of
/// [`RegistryHandle::resident_reason`] most likely to be *removed* by someone reasoning
/// correctly from the wrong premise. The whole argument is on that function; this pins it.
///
/// §7.2 is what makes it right: an orphan is a node that *"requires user resolution"*, and the
/// resolution is the operator's over this socket. The supervisor is the handle on it — with a
/// pid on record, a confirmed KillTree signals the surviving process. Exiting instead of
/// holding does not avoid the problem, it discards the only means of fixing it.
///
/// `detached_supervisor.rs` covers the same claim end to end with a process it really started;
/// this covers it where the predicate lives, so the reasoning is refuted in the unit suite
/// rather than eight seconds into an integration run.
#[test]
fn an_orphaned_node_holds_the_supervisor_resident_because_it_is_the_operators_to_resolve() {
    let fx = restart_fx_with(
        "handler-resident-orphan",
        vec![
            intent("lost", None, "claude", 0),
            spawned("lost", 55),
            state("lost", NodeState::Running),
        ],
    );
    assert_eq!(
        fx.handle
            .live
            .read(|r| r.tree().get(&id("lost")).unwrap().reap_state),
        ReapState::Orphaned,
        "the fixture's premise: the boot marked it",
    );
    assert_eq!(
        fx.handle.residency(),
        Some(marion_core::proto::ResidentReason::NonTerminalNode),
        "§7.2: marion does not know, and the operator is the one who resolves that",
    );
    assert!(!fx.handle.idle_exit_eligible());

    // And it is released by the same thing that releases any node: a recorded fate. Which also
    // retracts the marking (`marion_core::registry::Replay::apply`), so the two agree.
    append(
        &fx.path,
        &line(
            3,
            1_003,
            RecordKind::Exited(Exited {
                agent_id: id("lost"),
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "exited cleanly".into(),
                },
            }),
        ),
    );
    fx.handle.live.refresh();
    assert_eq!(fx.handle.residency(), None);
    assert_eq!(
        fx.handle
            .live
            .read(|r| r.tree().get(&id("lost")).unwrap().reap_state),
        ReapState::Live,
        "the marking went with the premise it rested on",
    );
}

/// **A reap intent stops holding the supervisor once the death it was about is observed.**
///
/// §7.2's crash window is *"the supervisor died before the kill landed"*, and §5.7 holds the
/// supervisor for it because a process may still be running. Once a terminal record for that
/// node is on the journal the window is shut: the process was observed dead, which is the very
/// check §7.2 says resolves the intent. Holding on the stale intent after that is a supervisor
/// that can never exit — reachable in one supervisor's life, as this fixture's order shows:
/// the reap's signal went out and could not be observed (so no `ReapConfirmed` was written),
/// and the operator then confirmed a `session/quit` KillTree, which did observe it.
#[test]
fn a_reap_intent_stops_holding_the_supervisor_once_the_node_is_observed_dead() {
    let fx = fx_with(
        "handler-quit-reap-intent-then-killed",
        vec![
            intent("idle", None, "claude", 0),
            spawned("idle", 91),
            state("idle", NodeState::Idle),
            RecordKind::ReapIntent(ReapIntent {
                agent_id: id("idle"),
                reason: "session/quit reaped an idle node before detaching busy work".into(),
            }),
            RecordKind::KillConfirmed(KillConfirmed {
                agent_id: id("idle"),
                exit: ProcessExit {
                    code: None,
                    signal: Some(9),
                    description: "confirmed session/quit killed the node's process tree".into(),
                },
            }),
        ],
    );
    assert_eq!(
        fx.handle.residency(),
        None,
        "every node's death is on the record; nothing is left for this supervisor to hold",
    );
    assert!(fx.handle.idle_exit_eligible());
}

/// **NC — EOF has no default disposition.** An idle node is the sharp control because the
/// explicit default would reap it; `gone(SocketClosed)` must leave both its runtime trace and
/// every journal byte alone.
#[test]
fn a_dropped_socket_is_not_any_quit_disposition_and_does_not_apply_the_default() {
    let (fx, runtime) = recording_fx_with(
        "handler-quit-eof",
        vec![
            intent("idle", None, "claude", 0),
            spawned("idle", 77),
            state("idle", NodeState::Idle),
        ],
    );
    let before = std::fs::read(&fx.path).unwrap();
    fx.handle
        .gone(ConnId(44), &ClientGone::SocketClosed, &Departure::Eof);
    assert!(runtime.killed().is_empty());
    assert_eq!(std::fs::read(&fx.path).unwrap(), before);
    assert_eq!(
        fx.handle
            .live
            .read(|r| r.tree().get(&id("idle")).unwrap().reap_state),
        ReapState::Live
    );
}

/// Observes the journal at the instant of each signal, which no ordinary assertion can: the
/// finished file records *intent, confirm* for both a correct implementation and one that
/// appends the confirmation before sending anything. §4.3's order is what separates them, and
/// a confirmation that precedes its act is a durable claim marion never earned.
#[derive(Default)]
struct OrderingRuntime {
    path: Mutex<Option<PathBuf>>,
    at_signal: Mutex<Vec<Vec<String>>>,
}

impl QuitRuntime for OrderingRuntime {
    fn kill_process_tree_and_wait(&self, _pid: i32) -> bool {
        let path = lock(&self.path).clone().expect("the fixture set its path");
        lock(&self.at_signal).push(journal_tags(&path));
        true
    }
}

/// A runtime that signals and cannot observe death, which is the failure `run.rs`'s bounded
/// wait returns rather than asserting away.
struct UnobservableRuntime;

impl QuitRuntime for UnobservableRuntime {
    fn kill_process_tree_and_wait(&self, _pid: i32) -> bool {
        false
    }
}

/// **NC — (a)'s PID preflight refuses rather than signals into the dark.**
///
/// A confirmed node whose spawn has not resolved has no recorded PID. Proceeding would append
/// a durable kill intent for a process marion cannot address, which is the half-happened kill
/// the confirmed list exists to prevent — and the refusal is a `Conflict`, because the
/// operator's next move is to re-render, not to file a bug.
#[test]
fn kill_tree_refuses_a_confirmed_node_whose_pid_is_not_recorded_yet() {
    let (fx, runtime) = recording_fx_with(
        "handler-quit-kill-no-pid",
        vec![intent("root", None, "claude", 0)],
    );
    let before = std::fs::read(&fx.path).unwrap();

    let error = quit(
        &fx,
        marion_core::proto::QuitDisposition::KillTree {
            confirmed: vec![id("root")],
        },
    )
    .expect_err("a node with no PID cannot be proven to have been reached");
    assert_eq!(error.kind(), Some(FailureKind::Conflict));
    assert!(error.message.contains("no recorded PID"), "{error}");
    assert!(runtime.killed().is_empty(), "nothing was signalled");
    assert_eq!(std::fs::read(&fx.path).unwrap(), before);
}

/// **NC — each node's confirmation is appended after its signal, not before it.**
///
/// The finished journal is identical either way, so the assertion has to be made *during* the
/// signal. §7.2's recovery reads an intent without a confirmation as "marion may have killed
/// this"; a confirmation written first would make the opposite claim durable in the one window
/// where it is false.
#[test]
fn every_kill_is_signalled_before_its_confirmation_becomes_durable() {
    let runtime = Arc::new(OrderingRuntime::default());
    let fx = fx_with_runtime(
        "handler-quit-kill-order",
        vec![
            intent("root", None, "claude", 0),
            spawned("root", 301),
            state("root", NodeState::Running),
            intent("child", Some("root"), "codex-impl", 1),
            spawned("child", 302),
            state("child", NodeState::Running),
        ],
        runtime.clone(),
    );
    *lock(&runtime.path) = Some(fx.path.clone());

    quit(
        &fx,
        marion_core::proto::QuitDisposition::KillTree {
            confirmed: vec![id("root"), id("child")],
        },
    )
    .expect("the exact set was confirmed");

    let snapshots = lock(&runtime.at_signal).clone();
    assert_eq!(snapshots.len(), 2, "one signal per node");
    for (i, tags) in snapshots.iter().enumerate() {
        assert_eq!(
            tags.last().map(String::as_str),
            Some("KillIntent"),
            "node {i}'s intent is durable at the moment it is signalled"
        );
        assert_eq!(
            tags.iter().filter(|t| *t == "KillConfirmed").count(),
            i,
            "node {i} was not confirmed before it was signalled"
        );
    }
}

/// **NC — a node §7.2 already reaped is retired without a second signal.**
///
/// `ReapedIdle` is not `Exited`, so (a) must still account for it, but its process is already
/// gone. Signalling its recorded PID again would address whatever now owns that number, and
/// recording `signal: 9` would claim marion did something it did not do.
#[test]
fn kill_tree_retires_an_already_reaped_node_without_signalling_it_again() {
    let (fx, runtime) = recording_fx_with(
        "handler-quit-kill-reaped",
        vec![
            intent("reaped", None, "claude", 0),
            spawned("reaped", 501),
            state("reaped", NodeState::Idle),
            RecordKind::ReapIntent(ReapIntent {
                agent_id: id("reaped"),
                reason: "an earlier session/quit reaped it".into(),
            }),
            RecordKind::ReapConfirmed(ReapConfirmed {
                agent_id: id("reaped"),
            }),
            intent("live", None, "claude", 0),
            spawned("live", 502),
            state("live", NodeState::Running),
        ],
    );

    quit(
        &fx,
        marion_core::proto::QuitDisposition::KillTree {
            confirmed: vec![id("live"), id("reaped")],
        },
    )
    .expect("both non-terminal nodes were confirmed");
    assert_eq!(
        runtime.killed(),
        [502],
        "a ReapedIdle node has no process left to signal"
    );
    let replay = crate::journal::read_path(&fx.path).unwrap();
    assert_eq!(
        replay
            .get(&id("reaped"))
            .unwrap()
            .exit
            .as_ref()
            .unwrap()
            .signal,
        None,
        "the record does not claim a signal marion never sent"
    );
    assert_eq!(
        replay
            .get(&id("live"))
            .unwrap()
            .exit
            .as_ref()
            .unwrap()
            .signal,
        Some(9)
    );
}

/// `node/kill` over the handler, the way a connection sends it.
fn node_kill(fx: &Fx, agent: &str) -> Result<marion_core::proto::result::NodeKillResult, RpcError> {
    let out = crate::serve::sink(ConnId(9));
    fx.handle.hello_as_operator(ConnId(9));
    match fx.handle.call(
        ConnId(9),
        &Call::NodeKill(marion_core::proto::params::NodeKillParams {
            agent_id: id(agent),
        }),
        &out,
    )? {
        MethodResult::NodeKill(r) => Ok(r),
        other => panic!("wrong result: {}", other.method().as_str()),
    }
}

/// A live root with a recorded pid, as `journal::confirm_spawned` leaves one.
fn running_root(agent: &str, pid: i32) -> Vec<RecordKind> {
    vec![
        intent(agent, None, "claude", 0),
        spawned(agent, pid),
        state(agent, NodeState::Running),
    ]
}

/// **NC — an id the journal never named is `NotFound`, and nothing is written or signalled.**
#[test]
fn node_kill_refuses_an_unknown_node_before_any_side_effect() {
    let (fx, runtime) = recording_fx_with("handler-kill-unknown", running_root("root", 101));
    let before = std::fs::read(&fx.path).unwrap();

    let e = node_kill(&fx, "nobody").expect_err("no such node");
    assert_eq!(e.kind(), Some(FailureKind::NotFound), "{e}");
    assert!(e.message.contains("nobody"), "{e}");
    assert!(runtime.killed().is_empty());
    assert_eq!(std::fs::read(&fx.path).unwrap(), before);
}

/// **NC — a finished node is refused with its terminal state, and its old pid is never
/// signalled.** That number may belong to an unrelated process by now, and a second terminal
/// record would rewrite how the node ended.
#[test]
fn node_kill_refuses_an_exited_node_naming_its_state_and_signals_nothing() {
    let mut records = running_root("done", 101);
    records.push(RecordKind::Exited(Exited {
        agent_id: id("done"),
        status: ExitStatus::Failed,
        exit: ProcessExit {
            code: Some(1),
            signal: None,
            description: "it failed on its own".into(),
        },
    }));
    let (fx, runtime) = recording_fx_with("handler-kill-exited", records);
    let before = std::fs::read(&fx.path).unwrap();

    let e = node_kill(&fx, "done").expect_err("an exited node has nothing to end");
    assert_eq!(e.kind(), Some(FailureKind::Refused), "{e}");
    assert!(
        e.message.contains("Failed"),
        "the refusal names the state: {e}"
    );
    assert!(runtime.killed().is_empty(), "nothing was signalled");
    assert_eq!(std::fs::read(&fx.path).unwrap(), before);
}

/// **NC — a node still spawning has no pid to aim at**, so the refusal is a `Conflict` (retry
/// once the spawn resolves), exactly as `session/quit`'s KillTree preflight answers it.
#[test]
fn node_kill_refuses_a_node_with_no_recorded_pid_and_signals_nothing() {
    let (fx, runtime) = recording_fx_with(
        "handler-kill-no-pid",
        vec![intent("root", None, "claude", 0)],
    );
    let before = std::fs::read(&fx.path).unwrap();

    let e = node_kill(&fx, "root").expect_err("no pid, no provable signal");
    assert_eq!(e.kind(), Some(FailureKind::Conflict), "{e}");
    assert!(e.message.contains("no recorded PID"), "{e}");
    assert!(runtime.killed().is_empty());
    assert_eq!(std::fs::read(&fx.path).unwrap(), before);
}

/// A `ReapedIdle` node is retired, not signalled: its process is already gone (§7.2), and the
/// confirmation must not claim a signal marion never sent. The same rule KillTree applies.
#[test]
fn node_kill_retires_a_reaped_idle_node_without_signalling_it() {
    let (fx, runtime) = recording_fx_with(
        "handler-kill-reaped",
        vec![
            intent("reaped", None, "claude", 0),
            spawned("reaped", 501),
            state("reaped", NodeState::Idle),
            RecordKind::ReapIntent(ReapIntent {
                agent_id: id("reaped"),
                reason: "an earlier session/quit reaped it".into(),
            }),
            RecordKind::ReapConfirmed(ReapConfirmed {
                agent_id: id("reaped"),
            }),
        ],
    );

    // Owned, with its thread long past its process's end — which is what a reap leaves. That
    // race is settled and irrelevant here: nothing is signalled, so nothing is refused for it.
    fx.handle.claim(&id("reaped"), None, fx.path.clone());
    assert!(!fx.handle.process_ended(&id("reaped")));

    let r = node_kill(&fx, "reaped").expect("a reaped node can still be ended");
    assert_eq!(r.state, NodeState::Exited(ExitStatus::Cancelled));
    assert!(runtime.killed().is_empty(), "no process was left to signal");
    let replay = crate::journal::read_path(&fx.path).unwrap();
    let node = replay.get(&id("reaped")).unwrap();
    assert_eq!(node.state, NodeState::Exited(ExitStatus::Cancelled));
    assert_eq!(node.exit.as_ref().unwrap().signal, None);
}

/// **The happy path, with §6.7's order observed at the instant of the signal**: the intent is
/// durable when the process is signalled, the confirmation lands after, and the answer is the
/// state the journal now folds to — `Exited(Cancelled)` with marion named as the sender.
#[test]
fn node_kill_journals_its_intent_before_the_signal_and_its_confirmation_after() {
    let runtime = Arc::new(OrderingRuntime::default());
    let fx = fx_with_runtime(
        "handler-kill-order",
        running_root("root", 301),
        runtime.clone(),
    );
    *lock(&runtime.path) = Some(fx.path.clone());
    let before = journal_tags(&fx.path).len();

    let r = node_kill(&fx, "root").expect("a running node with a pid is ended");
    assert_eq!(r.state, NodeState::Exited(ExitStatus::Cancelled));

    let snapshots = lock(&runtime.at_signal).clone();
    assert_eq!(snapshots.len(), 1, "one signal for one node");
    assert_eq!(snapshots[0].last().map(String::as_str), Some("KillIntent"));
    assert_eq!(
        &journal_tags(&fx.path)[before..],
        ["KillIntent", "KillConfirmed"],
        "exactly one intent/confirm pair and nothing else"
    );
    let replay = crate::journal::read_path(&fx.path).unwrap();
    let node = replay.get(&id("root")).unwrap();
    assert_eq!(node.state, NodeState::Exited(ExitStatus::Cancelled));
    let exit = node.exit.as_ref().unwrap();
    assert_eq!(exit.signal, Some(9));
    assert!(
        exit.description.contains("node/kill"),
        "{}",
        exit.description
    );
}

/// **Ending one node is not quitting.** The other node keeps running, nothing records the
/// supervisor's exit, and §5.7 still holds the supervisor resident on the survivor — the
/// negative control against a helper extracted from KillTree that carried its "then exit" along.
#[test]
fn node_kill_ends_only_its_node_and_leaves_the_supervisor_resident() {
    let mut records = running_root("victim", 101);
    records.extend(running_root("survivor", 202));
    let (fx, runtime) = recording_fx_with("handler-kill-one-of-two", records);

    node_kill(&fx, "victim").expect("the victim is ended");
    assert_eq!(runtime.killed(), [101], "only the named node's pid");
    let replay = crate::journal::read_path(&fx.path).unwrap();
    assert_eq!(
        replay.get(&id("survivor")).unwrap().state,
        NodeState::Running
    );
    assert_eq!(
        fx.handle.residency(),
        Some(ResidentReason::NonTerminalNode),
        "the survivor keeps the supervisor resident"
    );
    assert!(!fx.handle.begin_idle_exit());
    assert!(!fx.handle.exiting());
    assert!(
        !journal_tags(&fx.path).contains(&"SupervisorExited".to_string()),
        "a per-node kill never records the supervisor's exit"
    );
}

/// **NC — a death marion cannot observe is not confirmed.** The intent stays outstanding for
/// §7.2's recovery and the caller is told the kill did not complete.
#[test]
fn node_kill_whose_death_cannot_be_observed_leaves_its_intent_unconfirmed() {
    let fx = fx_with_runtime(
        "handler-kill-unobservable",
        running_root("root", 401),
        Arc::new(UnobservableRuntime),
    );
    let e = node_kill(&fx, "root").expect_err("the death was not observed");
    assert_eq!(e.kind(), Some(FailureKind::Internal), "{e}");
    assert_eq!(journal_tags(&fx.path).last().unwrap(), "KillIntent");
}

/// **The thread race, from the kill's side.** A node this supervisor owns whose own thread has
/// already seen its process end is finishing on its own: its thread is about to write the
/// terminal record it observed. Signalling now would aim at a reaped pid and put a second
/// terminal record beside the thread's, so the kill is refused before any side effect.
#[test]
fn node_kill_refuses_an_owned_node_whose_process_already_ended_on_its_own() {
    let (fx, runtime) = recording_fx_with("handler-kill-ended-first", running_root("root", 101));
    fx.handle.claim(&id("root"), None, fx.path.clone());
    fx.handle.mark_started(&id("root"), 101);
    assert!(
        !fx.handle.process_ended(&id("root")),
        "nobody asked marion to end it, so its thread records its own exit"
    );
    let before = std::fs::read(&fx.path).unwrap();

    let e = node_kill(&fx, "root").expect_err("its thread is already recording its end");
    assert_eq!(e.kind(), Some(FailureKind::Conflict), "{e}");
    assert!(runtime.killed().is_empty(), "nothing was signalled");
    assert_eq!(std::fs::read(&fx.path).unwrap(), before);
}

/// **The thread race, from the thread's side.** Once `node/kill` has claimed an owned node, the
/// node's thread is told at its process's end that marion ended it — so it writes no `Exited`
/// of its own over the `KillConfirmed` (which would refold the node to `Failed`) and records its
/// outcome as the cancellation it was.
#[test]
fn an_owned_node_that_marion_killed_is_told_so_when_its_thread_sees_the_process_end() {
    let (fx, runtime) = recording_fx_with("handler-kill-thread-told", running_root("root", 101));
    fx.handle.claim(&id("root"), None, fx.path.clone());
    fx.handle.mark_started(&id("root"), 101);

    node_kill(&fx, "root").expect("an owned running node is ended");
    assert_eq!(runtime.killed(), [101]);
    assert!(
        fx.handle.process_ended(&id("root")),
        "the thread learns its process ended because marion ended it"
    );
}

/// A node this supervisor does not own (an orphan, or a node another process recorded) is
/// still ended through its recorded pid; there is simply no thread here to tell.
#[test]
fn process_ended_for_a_node_nobody_owns_is_never_attributed_to_a_kill() {
    let (fx, _) = recording_fx_with("handler-kill-unowned", running_root("root", 101));
    assert!(!fx.handle.process_ended(&id("root")));
}

/// **One supervisor is one writer, across every RPC it serves — `seq` and `mono_ns` say so or
/// they are decoration.**
///
/// `JournalRecord.seq` is documented as *"this writer's ordinal, from 0, gapless by
/// construction"* and `mono_ns` as monotonic since the writer started, existing (§4.2) to anchor
/// a record against `pty.cast`. The handler used to open a `Journal` per `session/quit` with a
/// fresh `writer_id`, which made both untrue in a way nothing could see: replay seeds a writer's
/// expected ordinal from the first record it reads, so a crowd of one-RPC writers raises **no**
/// `SeqGap` — it just quietly stops being a timeline.
///
/// Three call paths, three RPCs, one file: the reap, the kill, and the supervisor's own exit
/// record. Reading the writers and ordinals back off the bytes is the only way to see it, since
/// the tree replay is identical either way — which is exactly why it went unnoticed.
#[test]
fn records_written_across_separate_rpcs_share_one_writer_and_one_rising_sequence() {
    let (fx, runtime) = recording_fx_with(
        "handler-one-writer",
        vec![
            intent("idle", None, "claude", 0),
            spawned("idle", 71),
            state("idle", NodeState::Idle),
            intent("busy", None, "claude", 0),
            spawned("busy", 72),
            state("busy", NodeState::Running),
        ],
    );
    let seeded = journal_records(&fx.path).len();

    // RPC 1 — reaps the idle root, detaches the busy one.
    quit(&fx, marion_core::proto::QuitDisposition::ReapIdleDetachBusy)
        .expect("one idle root is reapable");
    // RPC 2 — a different handler method, over the same registry. The reaped node is still
    // non-terminal by `state`, so §7.3.2 requires it in the confirmed set.
    quit(
        &fx,
        marion_core::proto::QuitDisposition::KillTree {
            confirmed: vec![id("idle"), id("busy")],
        },
    )
    .expect("the exact non-terminal set was confirmed");
    // RPC 3 — not a client call at all, and the third place that used to mint its own writer.
    assert!(fx.handle.begin_idle_exit());
    assert_eq!(
        runtime.killed(),
        [71, 72],
        "the reaped root was not re-signalled"
    );

    let written: Vec<_> = journal_records(&fx.path).into_iter().skip(seeded).collect();
    assert_eq!(
        journal_tags(&fx.path)[seeded..],
        [
            "ReapIntent",
            "ReapConfirmed",
            "KillIntent",
            "KillConfirmed",
            "KillIntent",
            "KillConfirmed",
            "SupervisorExited",
        ],
        "the fixture is only interesting if all three call paths really wrote"
    );

    let writers: std::collections::BTreeSet<_> =
        written.iter().map(|r| r.writer.0.clone()).collect();
    assert_eq!(
        writers.len(),
        1,
        "three RPCs, one supervisor process, one writer identity; got {writers:?}"
    );
    assert_ne!(
        writers.iter().next().unwrap(),
        "w",
        "and it is the supervisor's own identity, not the fixture's seeded one"
    );
    assert_eq!(
        written.iter().map(|r| r.seq).collect::<Vec<_>>(),
        [0, 1, 2, 3, 4, 5, 6],
        "`seq` continues across RPCs. Restarting at 0 per call raises no `SeqGap` — replay \
         seeds a new writer's expectation from whatever ordinal it first sees — so this \
         assertion is the only thing that can catch it"
    );
    assert!(
        written.windows(2).all(|w| w[0].mono_ns <= w[1].mono_ns),
        "one writer, one `Instant` origin, so §4.2's `pty.cast` anchor advances rather than \
         resetting: {:?}",
        written.iter().map(|r| r.mono_ns).collect::<Vec<_>>()
    );
}

/// **NC — a kill marion cannot observe dead is a refusal, not a confirmation.**
///
/// This is the one path `run.rs`'s bounded wait exists to produce, and it is the path that
/// must not end in an exit: an unconfirmed intent is exactly what §7.2's recovery needs to
/// find, and a supervisor that left anyway would take that recovery with it.
#[test]
fn a_kill_that_cannot_be_observed_dead_leaves_the_intent_unconfirmed_and_no_exit() {
    let fx = fx_with_runtime(
        "handler-quit-kill-unobserved",
        vec![
            intent("root", None, "claude", 0),
            spawned("root", 909),
            state("root", NodeState::Running),
        ],
        Arc::new(UnobservableRuntime),
    );

    let error = quit(
        &fx,
        marion_core::proto::QuitDisposition::KillTree {
            confirmed: vec![id("root")],
        },
    )
    .expect_err("marion did not observe the process dead");
    assert_eq!(error.kind(), Some(FailureKind::Internal));
    let tags = journal_tags(&fx.path);
    assert_eq!(tags.last().map(String::as_str), Some("KillIntent"));
    assert!(
        !tags.iter().any(|t| t == "KillConfirmed"),
        "nothing confirmed a death nobody saw: {tags:?}"
    );
    assert!(!fx.handle.idle_exit_eligible());
    assert!(!fx.handle.begin_idle_exit());
    assert!(!fx.handle.exiting());
}

/// **NC — (c) refuses each busy class on that node's own state.**
///
/// Every refusal class here is a *root*, so §7.2's "a node a spawn is blocked on" guard cannot
/// stand in for the state predicate. Without this, `reaping` could test nothing but parentage
/// and still detach every busy node in a tree-shaped fixture.
#[test]
fn reap_idle_detach_busy_refuses_each_busy_root_on_its_own_state() {
    let (fx, runtime) = recording_fx_with(
        "handler-quit-reap-roots",
        vec![
            intent("idle", None, "claude", 0),
            spawned("idle", 1),
            state("idle", NodeState::Idle),
            intent("running", None, "claude", 0),
            spawned("running", 2),
            state("running", NodeState::Running),
            intent("permission", None, "claude", 0),
            spawned("permission", 3),
            state("permission", NodeState::Blocked(BlockReason::Permission)),
            intent("elicitation", None, "claude", 0),
            spawned("elicitation", 4),
            state("elicitation", NodeState::Blocked(BlockReason::Elicitation)),
            intent("descendants", None, "claude", 0),
            spawned("descendants", 5),
            state("descendants", NodeState::Blocked(BlockReason::Descendants)),
            intent("spawning", None, "claude", 0),
        ],
    );

    let marion_core::proto::QuitOutcome::ReapedAndDetached {
        reaped, detached, ..
    } = quit(&fx, marion_core::proto::QuitDisposition::ReapIdleDetachBusy)
        .expect("no busy root blocks the reap of an idle one")
        .outcome
    else {
        panic!("reap returned another disposition's outcome")
    };
    assert_eq!(reaped, [id("idle")]);
    assert_eq!(
        detached,
        [
            id("running"),
            id("permission"),
            id("elicitation"),
            id("descendants"),
            id("spawning"),
        ]
    );
    assert_eq!(runtime.killed(), [1], "only the idle root was signalled");
}

/// **NC — an idle root already under, or past, a reap is not reaped a second time.**
///
/// An unconfirmed intent means some other actor may already be mid-reap, and a `ReapedIdle`
/// node has no process left; either way a second intent/confirm pair would journal an act that
/// did not happen to a process that is not there.
#[test]
fn reap_idle_detach_busy_skips_an_idle_root_already_under_or_past_a_reap() {
    let (fx, runtime) = recording_fx_with(
        "handler-quit-reap-twice",
        vec![
            intent("fresh", None, "claude", 0),
            spawned("fresh", 10),
            state("fresh", NodeState::Idle),
            intent("intended", None, "claude", 0),
            spawned("intended", 20),
            state("intended", NodeState::Idle),
            RecordKind::ReapIntent(ReapIntent {
                agent_id: id("intended"),
                reason: "someone else decided to reap it".into(),
            }),
            intent("already", None, "claude", 0),
            spawned("already", 30),
            state("already", NodeState::Idle),
            RecordKind::ReapIntent(ReapIntent {
                agent_id: id("already"),
                reason: "an earlier session/quit reaped it".into(),
            }),
            RecordKind::ReapConfirmed(ReapConfirmed {
                agent_id: id("already"),
            }),
        ],
    );

    let marion_core::proto::QuitOutcome::ReapedAndDetached {
        reaped, detached, ..
    } = quit(&fx, marion_core::proto::QuitDisposition::ReapIdleDetachBusy)
        .expect("one idle root was reapable")
        .outcome
    else {
        panic!("reap returned another disposition's outcome")
    };
    assert_eq!(reaped, [id("fresh")]);
    assert_eq!(runtime.killed(), [10]);
    assert_eq!(
        detached,
        [id("intended")],
        "a ReapedIdle node is no longer something a client can be detached from"
    );
}

/// **NC — (b) names the nodes an operator is walking away from, and not the ones that finished.**
///
/// A `detached` list padded with terminal nodes tells the operator that work is still out there
/// when it is not, which is the same lie as omitting a live one, in the other direction.
#[test]
fn detach_names_only_the_nodes_that_are_still_someones_agent() {
    let fx = fx_with(
        "handler-quit-detach-list",
        vec![
            intent("live", None, "claude", 0),
            spawned("live", 61),
            state("live", NodeState::Idle),
            intent("done", None, "claude", 0),
            spawned("done", 62),
            RecordKind::Exited(Exited {
                agent_id: id("done"),
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "already done".into(),
                },
            }),
        ],
    );

    let marion_core::proto::QuitOutcome::Detached {
        detached,
        gate_exposed,
        ..
    } = quit(&fx, marion_core::proto::QuitDisposition::DetachAll)
        .expect("detach is implemented")
        .outcome
    else {
        panic!("detach returned another disposition's outcome")
    };
    assert_eq!(detached, [id("live")]);
    assert_eq!(gate_exposed, [id("live")]);
}

/// **NC — a departure decides nothing, and *"nothing"* is about nodes and about time, not
/// about the supervisor's right to leave an empty project.**
///
/// §7.3.1 is a rule about **agents**: *"a crashed, SIGKILLed, or otherwise vanished client MUST
/// leave every node exactly as it was"*, and *"nothing is journaled about the client's death"*.
/// Both are asserted here, byte for byte.
///
/// What §7.3.1 does **not** say is that the supervisor must outlive its own emptiness. §5.7's
/// permission is two clauses — *"zero clients and zero non-terminal nodes"* — and neither
/// mentions a quit. Reading one in is what left a supervisor immortal after every client that
/// died rather than resigned, which is not a stricter reading of the crash invariant but a leak
/// wearing its name; `handler-exit-after-socket-closed` above is the same fact stated
/// positively.
///
/// The distinction that *does* survive is timing, and it is asserted here: a departure marion
/// could not read does not waive §5.7's grace, because for all marion knows the replacement
/// window is already opening. Only an explicit `session/quit` does.
#[test]
fn a_departure_decides_nothing_and_does_not_shorten_the_wait() {
    let fx = fx_with(
        "handler-quit-eof-eligibility",
        vec![
            intent("done", None, "claude", 0),
            spawned("done", 7),
            RecordKind::Exited(Exited {
                agent_id: id("done"),
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "already done".into(),
                },
            }),
        ],
    );
    let before = std::fs::read(&fx.path).unwrap();

    fx.handle.connected(ConnId(3));
    fx.handle
        .gone(ConnId(3), &ClientGone::SocketClosed, &Departure::Eof);
    assert_eq!(
        std::fs::read(&fx.path).unwrap(),
        before,
        "§7.3.1: nothing is journaled about a client's death, and no node moved"
    );
    assert!(
        !fx.handle.exiting(),
        "`gone` itself never commits to an exit; §5.7 splits the decision from the record"
    );
    assert!(
        !fx.handle.idle_exit_grace_waived(),
        "§7.3.1: a close said nothing, so it cannot have said 'and do not wait'"
    );
    assert!(
        fx.handle.idle_exit_eligible(),
        "§5.7's two clauses are both satisfied; the accept loop still owes the whole grace"
    );
}

/// **NC — `exiting` follows the exit record; it does not precede it.**
///
/// §5.7's record is what distinguishes *finished and left* from *died*. A supervisor that
/// committed to exiting and only then failed to journal would produce exactly the ambiguity
/// the record exists to remove, and would do it on the one path — a journal marion cannot write
/// — where the evidence is least recoverable.
#[test]
fn the_supervisor_does_not_commit_to_exiting_before_its_record_is_durable() {
    let fx = fx_with(
        "handler-quit-exit-undurable",
        vec![
            intent("done", None, "claude", 0),
            spawned("done", 8),
            RecordKind::Exited(Exited {
                agent_id: id("done"),
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "already done".into(),
                },
            }),
        ],
    );
    let marion_core::proto::QuitOutcome::Detached { supervisor, .. } =
        quit(&fx, marion_core::proto::QuitDisposition::DetachAll)
            .expect("detach is implemented")
            .outcome
    else {
        panic!("detach returned another disposition's outcome")
    };
    assert_eq!(
        supervisor,
        marion_core::proto::SupervisorDisposition::Exiting
    );
    assert!(fx.handle.idle_exit_eligible());

    // A path the journal cannot be appended to. Nothing else about the decision changes, so
    // the only reason to stay is the one under test.
    std::fs::remove_file(&fx.path).unwrap();
    std::fs::create_dir(&fx.path).unwrap();
    assert!(!fx.handle.begin_idle_exit());
    assert!(
        !fx.handle.exiting(),
        "an exit that could not be recorded did not happen"
    );
}

/// A method that is specified and not built says so — `Unimplemented`, not `Unsupported`, and
/// not silence.
#[test]
fn a_specified_but_unbuilt_method_is_refused_with_the_milestone_named() {
    let w = Wired::new("handler-unimpl");
    let mut c = w.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    call(
        &mut c,
        Call::NodeRename(marion_core::proto::params::NodeRenameParams {
            agent_id: id("root"),
            name: "impl".into(),
        }),
        1,
    );
    let Frame::Response(resp) = next_frame(&mut r) else {
        panic!("expected a response")
    };
    let marion_core::proto::Outcome::Error(e) = resp.outcome else {
        panic!("expected a refusal")
    };
    assert_eq!(e.kind(), Some(FailureKind::Unimplemented));
    assert!(e.message.contains("node/rename"), "{e}");
}

/// **NC — a subscriber is told about a change exactly once, and a subscription that starts late
/// is not told about what its own snapshot already contained.**
///
/// §7.3.3's seam, as a test rather than as an argument: the snapshot and the point notifications
/// begin from are taken under one lock from one read, so there is no instant between them for an
/// event to be lost in or duplicated across.
#[test]
fn a_snapshot_and_its_subscription_meet_exactly_with_no_gap_and_no_overlap() {
    let w = Wired::new("handler-seam");
    // Two clients: one subscribes before the child appears, one after.
    let mut early = w.dial();
    let mut er = std::io::BufReader::new(early.try_clone().unwrap());
    call(
        &mut early,
        Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
        1,
    );
    next_frame(&mut er);

    append(
        &w.fx.path,
        &line(1, 2_000, intent("child", Some("root"), "codex-impl", 1)),
    );
    assert!(until(
        || w.fx.handle.live.read(|r| r.tree().nodes().len()) == 2
    ));

    let mut late = w.dial();
    let mut lr = std::io::BufReader::new(late.try_clone().unwrap());
    call(
        &mut late,
        Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
        1,
    );

    // The late subscriber's snapshot already has the child…
    let Frame::Response(resp) = next_frame(&mut lr) else {
        panic!("expected a response")
    };
    let marion_core::proto::Outcome::Result(body) = resp.outcome else {
        panic!("expected a result")
    };
    let MethodResult::TreeSubscribe(snap) = marion_core::proto::Method::TreeSubscribe
        .decode_result(&body)
        .unwrap()
    else {
        panic!("wrong result type")
    };
    assert_eq!(snap.nodes.len(), 2, "the snapshot is current, not stale");

    // …and the early subscriber was told about it, as the flush that the late subscribe
    // performed on its way in.
    let Frame::Notification(n) = next_frame(&mut er) else {
        panic!("the early subscriber must hear about the child")
    };
    assert_eq!(n.event.method(), "tree/node-added");

    // Now a change after both are subscribed reaches both, once each.
    append(
        &w.fx.path,
        &line(
            2,
            3_000,
            RecordKind::Exited(Exited {
                agent_id: id("child"),
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "clean exit".into(),
                },
            }),
        ),
    );
    assert!(until(|| w.fx.handle.flush() == 0
        && w.fx.handle.live.read(|r| r
            .tree()
            .get(&id("child"))
            .unwrap()
            .state
            .is_exited())));
    for r in [&mut er, &mut lr] {
        let Frame::Notification(n) = next_frame(r) else {
            panic!("both subscribers hear the exit")
        };
        let Event::NodeState { state, ts, .. } = n.event else {
            panic!("expected node/state")
        };
        assert_eq!(state, NodeState::Exited(ExitStatus::Ok));
        assert_eq!(ts, SystemTime::from_unix_millis(3_000));
    }
    // And exactly once: a second flush produces nothing, so nothing further arrives.
    assert_eq!(w.fx.handle.flush(), 0, "a told transition is not re-told");
}

/// **Nobody subscribed, nothing walked; nothing new, nothing re-walked.** `collect` visits
/// every node the journal ever recorded and the accept loop flushes on every pass, which at
/// 100k records was half a core on an idle supervisor with no client. The told-set is still
/// right for the first subscriber, because `subscribe` catches it up itself.
#[test]
fn a_flush_walks_the_tree_only_for_a_subscriber_and_only_when_it_changed() {
    let fx = fx_with(
        "handler-flush-guard",
        vec![
            intent("root", None, "claude", 0),
            intent("b", None, "claude", 0),
        ],
    );
    let h = &fx.handle;
    for _ in 0..3 {
        assert_eq!(h.flush(), 0);
    }
    assert_eq!(
        lock(&h.shared).collects,
        0,
        "no subscriber, yet the tree was walked"
    );

    let (out, captured) = crate::serve::capture(ConnId(77));
    let snapshot = h.subscribe(&out);
    assert_eq!(
        snapshot.nodes.len(),
        2,
        "the snapshot is complete without earlier walks"
    );
    let walked = lock(&h.shared).collects;
    for _ in 0..3 {
        assert_eq!(h.flush(), 0);
    }
    assert_eq!(
        lock(&h.shared).collects,
        walked,
        "an unchanged journal was walked again"
    );

    append(&fx.path, &line(2, 3_000, intent("c", None, "claude", 0)));
    h.live.refresh();
    assert_eq!(h.flush(), 1, "the new node is told once");
    assert!(captured.try_recv().is_ok(), "and reaches the subscriber");
    assert_eq!(h.flush(), 0);
}

/// **An idle supervisor's accept loop does not run between changes.** Every pass asks the idle
/// predicate, which refreshes the registry, so the registry's poll count is the pass count: the
/// follower has no safety poll of its own. The loop used to run every 5 ms.
#[test]
fn an_idle_supervisor_runs_no_accept_passes_between_changes() {
    let dir = scratch("handler-idle-passes");
    let path = dir.join("journal.jsonl");
    let live = Arc::new(LiveRegistry::follow(Registry::boot_path(&path).unwrap()));
    let handle = RegistryHandle::with_runtime(Arc::clone(&live), Arc::new(SystemQuitRuntime));
    let sock = std::path::PathBuf::from(format!("/tmp/mh-idle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sock);
    std::fs::create_dir_all(&sock).unwrap();
    let paths = crate::socket::socket_paths(&sock, Path::new("/p"), 1);
    let crate::socket::Acquired::Serving(serving) = crate::socket::acquire(&paths).unwrap() else {
        panic!("nothing was listening")
    };
    let server = crate::serve::Server::start_with_idle_grace(
        serving,
        handle as Arc<dyn crate::serve::Handle>,
        std::time::Duration::from_secs(3600),
    );
    std::thread::sleep(std::time::Duration::from_millis(100));
    let before = live.read(|r| r.polls());
    std::thread::sleep(std::time::Duration::from_millis(500));
    let passes = live.read(|r| r.polls()) - before;
    server.stop();
    let _ = std::fs::remove_dir_all(&sock);
    assert_eq!(
        passes, 0,
        "{passes} accept passes in 500 ms with nothing due and nothing changed"
    );
}

// ---------------------------------------------------------------------------------------
// §2's `node/attach` — §7.3.3's re-attach. `events.rs` owns the cursor and proves it loses and
// repeats nothing across a torn seam; what these assert is the thing that module cannot: that
// the cursor is wired to a **client**, over a socket, on a connection the client already had.
// ---------------------------------------------------------------------------------------

use crate::events::{Draft, EventWriter};
use marion_core::event::{Payload, PayloadKind};
use marion_core::ir::Source;

/// Where a node's stream lives, derived the way the handler derives it — from the journal.
fn events_of(fx: &Fx, agent: &str) -> std::path::PathBuf {
    fx.path
        .parent()
        .unwrap()
        .join("agents")
        .join(agent)
        .join("events.jsonl")
}

/// Append `n` events a test can recognise by name, continuing whatever ordinal the file is at.
fn say(path: &Path, agent: &str, tags: &[&str]) {
    let mut w = EventWriter::open_path(path, &id(agent)).expect("the stream opens");
    for t in tags {
        w.record(Draft::observed(Payload::Raw((*t).into()), Source::Protocol));
    }
    w.sync()
        .expect("the bytes are on disk before the test looks for them");
}

/// The `node/event` notifications a client has been sent, as `(agent_seq, payload text)`.
fn heard(events: &[Event]) -> Vec<(u64, String)> {
    events
        .iter()
        .map(|e| {
            let Event::NodeEvent {
                agent_seq, payload, ..
            } = e
            else {
                panic!("expected node/event, got {}", e.method())
            };
            (
                *agent_seq,
                payload["Raw"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// Send `node/attach` and read every frame up to and including its response.
///
/// The notifications come **first** by construction — the handler sends the replay before it
/// returns — so a helper that read the response first would hang, which is itself the assertion
/// that the ordering is what `node_attach`'s doc says.
fn attach(
    c: &mut std::os::unix::net::UnixStream,
    r: &mut std::io::BufReader<std::os::unix::net::UnixStream>,
    agent: &str,
    rid: i64,
) -> (Vec<Event>, marion_core::proto::Outcome) {
    call(
        c,
        Call::NodeAttach(marion_core::proto::params::NodeAttachParams {
            agent_id: id(agent),
            pane_stream: None,
        }),
        rid,
    );
    let mut notes = Vec::new();
    loop {
        match next_frame(r) {
            Frame::Notification(n) => notes.push(n.event),
            Frame::Response(resp) => return (notes, resp.outcome),
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

fn attached_ok(
    outcome: marion_core::proto::Outcome,
) -> marion_core::proto::result::NodeAttachResult {
    let marion_core::proto::Outcome::Result(body) = outcome else {
        panic!("node/attach was refused: {outcome:?}")
    };
    let MethodResult::NodeAttach(r) = marion_core::proto::Method::NodeAttach
        .decode_result(&body)
        .expect("the result decodes")
    else {
        panic!("wrong result type")
    };
    r
}

fn refusal(outcome: marion_core::proto::Outcome) -> RpcError {
    match outcome {
        marion_core::proto::Outcome::Error(e) => e,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// -----------------------------------------------------------------------------------------
// §5.3's display plane, end to end over the socket
// -----------------------------------------------------------------------------------------

/// A real pty running `script`, not yet registered as `agent`'s pane.
///
/// A **real child on a real pty**, not a fake: everything these tests are about — that a
/// keystroke reaches the process, that a resize reaches it as a `SIGWINCH`, that detaching
/// leaves it running — is a claim about a process, and a stub would let all four pass while
/// none of them was true.
fn unregistered_pane(w: &Wired, agent: &str, script: &str) -> Arc<crate::pty::PtyHost> {
    use crate::pty::{PtyHost, PtyMaster, StdinPlan, WinSize, spawn_pty};
    let size = WinSize::new(80, 24);
    let master = PtyMaster::open(size).expect("a pty");
    let cast = w.dir.join(format!("{agent}.cast"));
    let host = PtyHost::start(
        id(agent),
        master,
        &cast,
        size,
        "xterm-256color",
        std::time::Instant::now(),
    )
    .expect("a recording");
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    cmd.env("TERM", "xterm-256color");
    let child = spawn_pty(
        marion_harness::ExecutionSurfaces::opaque()
            .display_plane()
            .expect("`opaque` owns a pty"),
        &mut cmd,
        host.master(),
        StdinPlan::TerminalSlave,
        None,
    )
    .expect("the child starts");
    host.adopt(child);
    Arc::new(host)
}

fn pane(w: &Wired, agent: &str, script: &str) -> Arc<crate::pty::PtyHost> {
    let host = unregistered_pane(w, agent, script);
    w.fx.handle.register_pane(&id(agent), Arc::clone(&host));
    host
}

fn drained_zero_output_pane(w: &Wired, agent: &str) -> Arc<crate::pty::PtyHost> {
    let host = pane(w, agent, "exit 0");
    assert!(
        until(|| host.poll_exited_unreaped().unwrap()),
        "the zero-output child did not exit"
    );
    w.fx.handle.closing_pane(&id(agent), &host);
    host.shutdown().unwrap();
    host.completed_replay_charge()
        .expect("the zero-output host retained its End");
    host
}

fn native_launch_binding(agent: &str) -> crate::native_bootstrap::NativeLaunchBinding {
    crate::native_bootstrap::NativeLaunchBinding::new(
        id(agent),
        PathBuf::from("/project"),
        crate::native_bootstrap::PeerIdentity::current_for_tty_test(),
        ConnId(500),
        crate::native_bootstrap::TerminalFingerprint::new(1, 2, 3),
        crate::native_bootstrap::TerminalGeometry {
            cols: 80,
            rows: 24,
            xpixel: 0,
            ypixel: 0,
        },
        crate::native_bootstrap::NativeLaunchDescriptor::new("atlas", "atlas", "atlas-native"),
        crate::native_bootstrap::context_hash(
            &crate::native_bootstrap::DirectNativeRequestContext::new(
                PathBuf::from("/project"),
                PathBuf::from("/project"),
                "atlas".into(),
                Vec::new(),
                "xterm".into(),
                1,
            ),
        ),
    )
}

fn native_claimant(conn: ConnId) -> crate::native_bootstrap::NativeClaimant {
    crate::native_bootstrap::NativeClaimant::new(
        conn,
        crate::native_bootstrap::PeerIdentity::current_for_tty_test(),
    )
}

#[test]
fn pending_native_reservation_gates_both_attach_modes_until_atomic_ticket_claim() {
    let w = Wired::new("handler-native-writer-priority");
    let host = unregistered_pane(&w, "root", "sleep 30");
    let launches = Arc::new(
        crate::native_bootstrap::PendingNativeLaunches::with_sources(
            Arc::new(SequenceRng::default()),
            Arc::new(ManualClock::default()),
            Duration::from_secs(60),
        ),
    );
    w.fx.handle
        .install_pending_native_launches(Arc::clone(&launches))
        .unwrap_or_else(|_| panic!("native launch authority installs once"));
    let binding = native_launch_binding("root");
    let pending =
        w.fx.handle
            .reserve_pending_native_launch(binding.clone())
            .unwrap();
    w.fx.handle.register_pane(&id("root"), Arc::clone(&host));
    let generation =
        w.fx.handle
            .publish_pending_native_launch(pending.receipt())
            .unwrap();

    for pane_stream_v1 in [false, true] {
        let (out, _rx) = crate::serve::capture(ConnId(601 + u64::from(pane_stream_v1)));
        let attach = if pane_stream_v1 {
            w.fx.handle
                .attach_pane_v1(&id("root"), &out)
                .unwrap()
                .0
                .expect("pane exists")
        } else {
            w.fx.handle
                .attach_pane(&id("root"), &out)
                .expect("pane exists")
        };
        assert!(!attach.writable);
        assert_eq!(host.writer(), None);
    }

    let claim =
        w.fx.handle
            .claim_pending_native_writer(
                pending.receipt().ticket(),
                binding.agent_id(),
                native_claimant(ConnId(700)),
            )
            .unwrap();
    assert_eq!(claim.host_generation(), generation);
    assert_eq!(claim.conn(), ConnId(700));
    assert_eq!(host.writer(), Some(ConnId(700)));
    assert_eq!(
        w.fx.handle.claim_pending_native_writer(
            pending.receipt().ticket(),
            binding.agent_id(),
            native_claimant(ConnId(701)),
        ),
        Err(crate::native_bootstrap::NativeLaunchClaimError::UnknownTicket)
    );
    w.fx.handle
        .gone(ConnId(700), &ClientGone::SocketClosed, &Departure::Eof);
    assert_eq!(
        host.writer(),
        None,
        "claim lease leaked after claimant departure"
    );
    host.shutdown().unwrap();
}

#[test]
fn unacknowledged_native_claim_releases_lease_and_preserves_ticket_for_retry() {
    let w = Wired::new("handler-native-claim-ack-rollback");
    let host = unregistered_pane(&w, "root", "sleep 30");
    let launches = Arc::new(
        crate::native_bootstrap::PendingNativeLaunches::with_sources(
            Arc::new(SequenceRng::default()),
            Arc::new(ManualClock::default()),
            Duration::from_secs(60),
        ),
    );
    w.fx.handle
        .install_pending_native_launches(Arc::clone(&launches))
        .unwrap_or_else(|_| panic!("native launch authority installs once"));
    let binding = native_launch_binding("root");
    let pending =
        w.fx.handle
            .reserve_pending_native_launch(binding.clone())
            .unwrap();
    let ticket = crate::native_bootstrap::NativeLaunchTicket::for_test([
        0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ]);
    w.fx.handle.register_pane(&id("root"), Arc::clone(&host));
    w.fx.handle
        .publish_pending_native_launch(pending.receipt())
        .unwrap();

    let prepared = w
        .fx
        .handle
        .prepare_pending_native_writer(&ticket, binding.agent_id(), native_claimant(ConnId(710)))
        .unwrap();
    assert_eq!(host.writer(), Some(ConnId(710)));
    drop(prepared);
    assert_eq!(host.writer(), None, "failed acknowledgement leaked lease");
    assert!(launches.has_pending(binding.agent_id()));

    let prepared = w
        .fx
        .handle
        .prepare_pending_native_writer(&ticket, binding.agent_id(), native_claimant(ConnId(711)))
        .unwrap();
    let claim = prepared.commit().unwrap();
    assert_eq!(claim.conn(), ConnId(711));
    assert_eq!(host.writer(), Some(ConnId(711)));
    w.fx.handle
        .gone(ConnId(711), &ClientGone::SocketClosed, &Departure::Eof);
    assert_eq!(host.writer(), None);
    host.shutdown().unwrap();
}

#[test]
fn blocked_native_claim_ack_does_not_block_an_unrelated_pane_attach() {
    let w = Wired::new("handler-native-blocked-claim-ack");
    let root = unregistered_pane(&w, "root", "sleep 30");
    let other = unregistered_pane(&w, "other", "sleep 30");
    w.fx.handle.register_pane(&id("other"), Arc::clone(&other));
    let launches = Arc::new(
        crate::native_bootstrap::PendingNativeLaunches::with_sources(
            Arc::new(SequenceRng::default()),
            Arc::new(ManualClock::default()),
            Duration::from_secs(60),
        ),
    );
    w.fx.handle
        .install_pending_native_launches(Arc::clone(&launches))
        .unwrap_or_else(|_| panic!("native launch authority installs once"));
    let binding = native_launch_binding("root");
    let pending =
        w.fx.handle
            .reserve_pending_native_launch(binding.clone())
            .unwrap();
    let ticket = crate::native_bootstrap::NativeLaunchTicket::for_test([
        0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ]);
    w.fx.handle.register_pane(&id("root"), Arc::clone(&root));
    w.fx.handle
        .publish_pending_native_launch(pending.receipt())
        .unwrap();
    let prepared = w
        .fx
        .handle
        .prepare_pending_native_writer(&ticket, binding.agent_id(), native_claimant(ConnId(720)))
        .unwrap();

    let (ack_waiting_tx, ack_waiting_rx) = std::sync::mpsc::sync_channel(1);
    let (ack_release_tx, ack_release_rx) = std::sync::mpsc::sync_channel(1);
    let blocked_ack = std::thread::spawn(move || {
        ack_waiting_tx.send(()).unwrap();
        // **Bounded, because this thread is joined.** The release always arrives on the happy
        // path, but a panic on the main thread before it — an assertion this test is built to
        // report — would leave this thread parked forever and `blocked_ack.join()` below
        // parked behind it, on a fixture that owns two real pty children. A bound turns that
        // into a failure the harness can print.
        let _ = ack_release_rx.recv_timeout(Duration::from_secs(10));
        drop(prepared);
    });
    ack_waiting_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("claim reached its blocked acknowledgement");

    let attach_handle = Arc::clone(&w.fx.handle);
    let (attach_done_tx, attach_done_rx) = std::sync::mpsc::sync_channel(1);
    let attach = std::thread::spawn(move || {
        let (out, _rx) = crate::serve::capture(ConnId(721));
        let writable = attach_handle
            .attach_pane(&id("other"), &out)
            .expect("unrelated pane stays visible")
            .writable;
        attach_done_tx.send(writable).unwrap();
    });
    let unrelated_writable = attach_done_rx.recv_timeout(Duration::from_millis(250));
    ack_release_tx.send(()).unwrap();
    blocked_ack.join().unwrap();
    attach.join().unwrap();
    assert_eq!(
        unrelated_writable,
        Ok(true),
        "claim acknowledgement held the global Panes lock"
    );
    assert_eq!(root.writer(), None);
    assert!(launches.has_pending(binding.agent_id()));
    w.fx.handle
        .gone(ConnId(721), &ClientGone::SocketClosed, &Departure::Eof);
    root.shutdown().unwrap();
    other.shutdown().unwrap();
}

#[test]
fn native_ticket_is_revoked_by_replacement_and_terminal_lifecycle() {
    let w = Wired::new("handler-native-writer-replacement");
    let launches = Arc::new(
        crate::native_bootstrap::PendingNativeLaunches::with_sources(
            Arc::new(SequenceRng::default()),
            Arc::new(ManualClock::default()),
            Duration::from_secs(60),
        ),
    );
    w.fx.handle
        .install_pending_native_launches(Arc::clone(&launches))
        .unwrap_or_else(|_| panic!("native launch authority installs once"));
    let binding = native_launch_binding("root");

    let old = unregistered_pane(&w, "root", "sleep 30");
    let pending =
        w.fx.handle
            .reserve_pending_native_launch(binding.clone())
            .unwrap();
    w.fx.handle.register_pane(&id("root"), Arc::clone(&old));
    w.fx.handle
        .publish_pending_native_launch(pending.receipt())
        .unwrap();
    let replacement = unregistered_pane(&w, "root", "sleep 30");
    w.fx.handle
        .register_pane(&id("root"), Arc::clone(&replacement));
    assert_eq!(
        w.fx.handle.claim_pending_native_writer(
            pending.receipt().ticket(),
            binding.agent_id(),
            native_claimant(ConnId(800))
        ),
        Err(crate::native_bootstrap::NativeLaunchClaimError::UnknownTicket)
    );
    assert_eq!(replacement.writer(), None);

    w.fx.handle.forget_pane(&id("root"));
    let closing_host = unregistered_pane(&w, "root", "sleep 30");
    let closing =
        w.fx.handle
            .reserve_pending_native_launch(binding.clone())
            .unwrap();
    w.fx.handle
        .register_pane(&id("root"), Arc::clone(&closing_host));
    w.fx.handle
        .publish_pending_native_launch(closing.receipt())
        .unwrap();
    w.fx.handle.closing_pane(&id("root"), &closing_host);
    assert_eq!(
        w.fx.handle.claim_pending_native_writer(
            closing.receipt().ticket(),
            binding.agent_id(),
            native_claimant(ConnId(801)),
        ),
        Err(crate::native_bootstrap::NativeLaunchClaimError::WrongBinding)
    );
    assert_eq!(closing_host.writer(), None);
    closing_host.shutdown().unwrap();
    replacement.shutdown().unwrap();
    old.shutdown().unwrap();
}

fn write_keys(s: &mut std::os::unix::net::UnixStream, agent: &str, bytes: &str) {
    let f = Frame::Input(marion_core::proto::ClientNotification::new(
        marion_core::proto::Input::NodePtyWrite {
            agent_id: id(agent),
            bytes: bytes.into(),
        },
    ));
    s.write_all(f.to_line().as_bytes()).unwrap();
    s.flush().unwrap();
}

fn write_opaque_keys(s: &mut std::os::unix::net::UnixStream, agent: &str, bytes: &[u8]) {
    let f = Frame::Input(marion_core::proto::ClientNotification::new(
        marion_core::proto::Input::NodePaneWrite(marion_core::proto::NodePaneWriteV1 {
            agent_id: id(agent),
            bytes: marion_core::proto::OpaquePaneBytesV1::new(bytes),
        }),
    ));
    s.write_all(f.to_line().as_bytes()).unwrap();
    s.flush().unwrap();
}

fn send_resize(s: &mut std::os::unix::net::UnixStream, agent: &str, cols: u16, rows: u16) {
    let f = Frame::Input(marion_core::proto::ClientNotification::new(
        marion_core::proto::Input::NodeResize {
            agent_id: id(agent),
            cols,
            rows,
        },
    ));
    s.write_all(f.to_line().as_bytes()).unwrap();
    s.flush().unwrap();
}

#[test]
fn a_forged_pane_ready_before_advertisement_is_inert() {
    let w = Wired::new("handler-pane-ready-dark");
    let host = pane(&w, "root", "sleep 30");
    assert_eq!(w.fx.handle.panes(), 1);
    assert_eq!(host.listeners(), 0);
    assert_eq!(w.fx.handle.attachments(), 0);
    assert_eq!(w.fx.handle.subscribers(), 0);

    let mut c = w.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    let forged = Frame::Input(marion_core::proto::ClientNotification::new(
        marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
            agent_id: id("root"),
            token: marion_core::proto::PaneReadyTokenV1::new([0x5a; 32]),
            cut: 0,
        }),
    ));
    c.write_all(forged.to_line().as_bytes()).unwrap();
    c.flush().unwrap();

    // A later request on the same connection is the processing barrier and the no-crash proof.
    // It must be the next frame: the forged notification produces no notification of its own.
    call(
        &mut c,
        Call::NodeGet(marion_core::proto::params::NodeGetParams::of(id("root"))),
        9,
    );
    assert!(matches!(next_frame(&mut r), Frame::Response(_)));
    assert_eq!(w.fx.handle.panes(), 1);
    assert_eq!(host.listeners(), 0);
    assert_eq!(w.fx.handle.attachments(), 0);
    assert_eq!(w.fx.handle.subscribers(), 0);
}

/// Forgetting the registry entry invalidates the host generation, including clones already
/// held by an attach path. The production change that must make this fail is removing the host
/// from `Panes` without first cancelling its pending pane replay: the stale clone can still
/// activate and emit the retained prefix after the node was forgotten.
#[test]
fn forgetting_a_pane_cancels_pending_replay_on_cloned_hosts() {
    let w = Wired::new("handler-pane-forget-replay");
    let host = pane(&w, "root", "printf 'prefix'; sleep 30");
    assert!(
        until(|| host.bytes_read() >= b"prefix".len() as u64),
        "the retained prefix never arrived"
    );
    let conn = ConnId(103);
    let (out, rx) = crate::serve::capture(conn);
    let descriptor = host
        .begin_pane_replay(conn, out.clone())
        .expect("the pre-forget replay is reserved");

    w.fx.handle.forget_pane(&id("root"));
    host.pane_ready(conn, &descriptor.token, descriptor.cut);

    assert!(
        rx.try_iter().next().is_none(),
        "a forgotten host activated its old retained replay"
    );
    assert!(
        host.begin_pane_replay(conn, out).is_none(),
        "a cloned forgotten host minted a new replay generation"
    );
    host.shutdown().unwrap();
}

/// A process may exit before its owner reaches the close callback. The production change that
/// must make this fail is removing or invalidating the host before `shutdown` joins the reader:
/// the late retained tail and terminal `End` then cannot be replayed from the completed pane.
#[test]
fn a_fast_exit_keeps_its_tail_and_end_available_for_late_internal_replay() {
    let w = Wired::new("handler-pane-fast-exit-retention");
    let host = pane(&w, "root", "printf 'fast-tail'");
    assert!(
        until(|| matches!(host.try_wait(), Ok(Some(_)))),
        "the controlled child did not exit"
    );

    let (progress, _progress_rx) = std::sync::mpsc::channel();
    let owner = NodeOwner {
        handle: Arc::clone(&w.fx.handle),
        task_id: None,
        repo: w.dir.clone(),
        tx: progress,
        identified: Mutex::new(None),
        announce_to: None,
        owes: Default::default(),
    };
    <NodeOwner as crate::root::PaneOwner>::closing(&owner, &id("root"), &host);
    host.shutdown().expect("the exited child and reader join");
    let charge = host
        .completed_replay_charge()
        .expect("the drained replay is eligible");
    <NodeOwner as crate::root::PaneOwner>::completed(&owner, &id("root"), &host, charge);
    assert_eq!(w.fx.handle.panes(), 1, "the completed pane stays retained");

    let conn = ConnId(1_106);
    let (out, rx) = crate::serve::capture(conn);
    let descriptor = host
        .begin_pane_replay(conn, out)
        .expect("the completed host remains available for a late internal replay");
    host.pane_ready(conn, &descriptor.token, descriptor.cut);

    let frames = rx
        .try_iter()
        .map(|line| {
            Frame::from_line(std::str::from_utf8(&line).expect("outbound is NDJSON"))
                .expect("outbound frame decodes")
        })
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    let mut ends = 0;
    let mut initial_geometry = 0;
    for (seq, frame) in frames.iter().enumerate() {
        let Frame::Notification(note) = frame else {
            panic!("captured outbound item is not a notification: {frame:?}")
        };
        let Event::NodePaneFrame(frame) = &note.event else {
            panic!("captured outbound item is not a pane frame: {frame:?}")
        };
        assert_eq!(frame.seq, seq as u64, "the replay sequence stays dense");
        match &frame.frame {
            marion_core::proto::PaneFrameKindV1::Output { bytes } => {
                output.extend_from_slice(bytes.as_bytes());
            }
            marion_core::proto::PaneFrameKindV1::End {} => ends += 1,
            marion_core::proto::PaneFrameKindV1::Resize { cols: 80, rows: 24 } => {
                initial_geometry += 1;
            }
            other => panic!("unexpected replay geometry: {other:?}"),
        }
    }
    assert_eq!(output, b"fast-tail");
    assert_eq!(initial_geometry, 1, "replay must seed geometry once");
    assert_eq!(ends, 1, "completion emits exactly one End: {frames:?}");
    assert!(matches!(
        frames.last(),
        Some(Frame::Notification(note))
            if matches!(&note.event, Event::NodePaneFrame(frame)
                if matches!(frame.frame, marion_core::proto::PaneFrameKindV1::End {}))
    ));
}

/// Completed retention expires from the completion instant, not the last replay. The exact
/// 300-second boundary is expired, and eviction must invalidate the host only after releasing
/// the global Panes lock so callbacks cannot deadlock the supervisor.
#[test]
fn completed_pane_ttl_is_exact_and_never_refreshed_by_replay() {
    let w = Wired::new("handler-pane-completed-ttl");
    let now = Arc::new(Mutex::new(std::time::Instant::now()));
    let clock_now = Arc::clone(&now);
    w.fx.handle
        .set_pane_clock_for_test(Arc::new(move || *lock(&clock_now)));
    let completed_at = *lock(&now);

    let host = pane(&w, "root", "printf 'ttl-tail'");
    assert!(
        until(|| host.poll_exited_unreaped().unwrap()),
        "the controlled child did not exit"
    );
    w.fx.handle.closing_pane(&id("root"), &host);
    host.shutdown().unwrap();
    let charge = host.completed_replay_charge().unwrap();
    w.fx.handle.completed_pane(&id("root"), &host, charge);
    assert_eq!(w.fx.handle.completed_usage_for_test(), (1, charge));

    *lock(&now) = completed_at + std::time::Duration::from_secs(299);
    let conn = ConnId(1_122);
    let (out, _rx) = crate::serve::capture(conn);
    assert!(
        host.begin_pane_replay(conn, out).is_some(),
        "the completed pane remains replayable before its TTL"
    );

    *lock(&now) = completed_at + std::time::Duration::from_secs(300);
    let invalidated_outside_panes = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&invalidated_outside_panes);
    let handle = Arc::clone(&w.fx.handle);
    host.set_invalidation_hook(Box::new(move || {
        observed.store(handle.panes.try_lock().is_ok(), Ordering::SeqCst);
    }));
    crate::serve::Handle::tick(w.fx.handle.as_ref());

    assert_eq!(w.fx.handle.completed_usage_for_test(), (0, 0));
    assert_eq!(w.fx.handle.panes(), 0);
    assert!(
        invalidated_outside_panes.load(Ordering::SeqCst),
        "TTL victim invalidation ran under the Panes lock"
    );
    let (out, _rx) = crate::serve::capture(ConnId(1_123));
    assert!(
        host.begin_pane_replay(ConnId(1_123), out).is_none(),
        "TTL eviction left a cloned completed host replayable"
    );
}

/// Cache retirement is connection-fatal for every negotiated pane stream. Silently cancelling
/// the cursor would leave the client waiting forever for an End that cannot arrive.
#[test]
fn completed_cache_eviction_visibly_departs_pending_replay() {
    let w = Wired::new("handler-pane-completed-visible-eviction");
    let now = Arc::new(Mutex::new(std::time::Instant::now()));
    let clock_now = Arc::clone(&now);
    w.fx.handle
        .set_pane_clock_for_test(Arc::new(move || *lock(&clock_now)));
    let host = drained_zero_output_pane(&w, "root");
    let charge = host.completed_replay_charge().unwrap();
    w.fx.handle.completed_pane(&id("root"), &host, charge);
    let conn = ConnId(1_202);
    let (out, _captured) = crate::serve::capture(conn);
    host.begin_pane_replay(conn, out.clone())
        .expect("pending completed replay");

    *lock(&now) += std::time::Duration::from_secs(300);
    w.fx.handle.prune_completed_panes();

    assert_eq!(
        out.departed(),
        Some(crate::serve::Departure::PaneReplayEvicted {
            agent_id: "root".into(),
        })
    );
}

#[test]
fn pane_ticks_with_no_completed_entries_do_not_scan_the_pane_map() {
    let w = Wired::new("handler-pane-empty-tick-fast-path");
    w.fx.handle.reset_completed_scan_count_for_test();

    for _ in 0..32 {
        crate::serve::Handle::tick(w.fx.handle.as_ref());
    }

    assert_eq!(
        w.fx.handle.completed_scan_count_for_test(),
        0,
        "the 5ms heartbeat scanned/allocated for an empty completed cache"
    );
}

#[test]
fn pane_ticks_scan_once_at_the_cached_completion_expiry() {
    let mut w = Wired::new("handler-pane-tick-expiry-deadline");
    w.server.take().expect("test server").stop();
    let now = Arc::new(Mutex::new(std::time::Instant::now()));
    let clock_now = Arc::clone(&now);
    w.fx.handle
        .set_pane_clock_for_test(Arc::new(move || *lock(&clock_now)));
    let completed_at = *lock(&now);
    let host = drained_zero_output_pane(&w, "root");
    w.fx.handle.completed_pane(&id("root"), &host, 1_024);
    w.fx.handle.reset_completed_scan_count_for_test();

    *lock(&now) = completed_at + std::time::Duration::from_secs(299);
    for _ in 0..32 {
        crate::serve::Handle::tick(w.fx.handle.as_ref());
    }
    assert_eq!(
        w.fx.handle.completed_scan_count_for_test(),
        0,
        "pre-expiry heartbeats scanned the completed cache"
    );

    *lock(&now) = completed_at + std::time::Duration::from_secs(300);
    crate::serve::Handle::tick(w.fx.handle.as_ref());
    assert_eq!(
        w.fx.handle.completed_scan_count_for_test(),
        1,
        "the exact deadline should perform one expiration scan"
    );
    assert_eq!(w.fx.handle.completed_usage_for_test(), (0, 0));
}

#[test]
fn completed_pane_count_cap_evicts_the_oldest_zero_output_entry() {
    let w = Wired::new("handler-pane-completed-count-cap");
    let now = std::time::Instant::now();
    w.fx.handle.set_pane_clock_for_test(Arc::new(move || now));
    w.fx.handle.set_completed_limit_for_test(2);
    let mut first = None;
    let mut charge_each = None;

    for index in 0..3 {
        let agent = format!("completed-{index:02}");
        let host = pane(&w, &agent, "exit 0");
        assert!(until(|| host.poll_exited_unreaped().unwrap()));
        w.fx.handle.closing_pane(&id(&agent), &host);
        host.shutdown().unwrap();
        let charge = host.completed_replay_charge().unwrap();
        assert!(charge > 0, "End-only completion must carry a real charge");
        if let Some(expected) = charge_each {
            assert_eq!(
                charge, expected,
                "identical zero-output hosts charge equally"
            );
        } else {
            charge_each = Some(charge);
        }
        if index == 0 {
            first = Some(Arc::clone(&host));
        }
        if index == 2 {
            let invalidated_outside_panes = Arc::new(AtomicBool::new(false));
            let observed = Arc::clone(&invalidated_outside_panes);
            let handle = Arc::clone(&w.fx.handle);
            first
                .as_ref()
                .unwrap()
                .set_invalidation_hook(Box::new(move || {
                    observed.store(handle.panes.try_lock().is_ok(), Ordering::SeqCst);
                }));
            w.fx.handle.completed_pane(&id(&agent), &host, charge);
            assert!(
                invalidated_outside_panes.load(Ordering::SeqCst),
                "oldest count victim was invalidated under Panes"
            );
        } else {
            w.fx.handle.completed_pane(&id(&agent), &host, charge);
        }
    }

    let charge_each = charge_each.unwrap();
    assert_eq!(w.fx.handle.completed_usage_for_test(), (2, 2 * charge_each));
    let panes = lock(&w.fx.handle.panes);
    assert!(
        !panes.hosts.contains_key(&id("completed-00")),
        "the oldest completion survived insertion past the injected cap"
    );
    assert!(panes.hosts.contains_key(&id("completed-02")));
    drop(panes);
    let first = first.unwrap();
    let (out, _rx) = crate::serve::capture(ConnId(1_124));
    assert!(
        first.begin_pane_replay(ConnId(1_124), out).is_none(),
        "the count victim remained replayable through a stale Arc"
    );
}

#[test]
fn individually_oversized_completion_preserves_the_existing_cache() {
    const BYTE_CAP: usize = 256 * 1024 * 1024;
    let w = Wired::new("handler-pane-completed-oversized");
    let now = std::time::Instant::now();
    w.fx.handle.set_pane_clock_for_test(Arc::new(move || now));

    let existing = drained_zero_output_pane(&w, "existing");
    w.fx.handle.completed_pane(&id("existing"), &existing, 1024);
    assert_eq!(w.fx.handle.completed_usage_for_test(), (1, 1024));

    let oversized = drained_zero_output_pane(&w, "oversized");
    let invalidated_outside_panes = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&invalidated_outside_panes);
    let handle = Arc::clone(&w.fx.handle);
    oversized.set_invalidation_hook(Box::new(move || {
        observed.store(handle.panes.try_lock().is_ok(), Ordering::SeqCst);
    }));
    w.fx.handle
        .completed_pane(&id("oversized"), &oversized, BYTE_CAP + 1);

    assert_eq!(w.fx.handle.completed_usage_for_test(), (1, 1024));
    let panes = lock(&w.fx.handle.panes);
    assert!(matches!(
        panes.hosts.get(&id("existing")),
        Some(PaneEntry::Completed { host, .. }) if Arc::ptr_eq(host, &existing)
    ));
    assert!(!panes.hosts.contains_key(&id("oversized")));
    drop(panes);
    assert!(
        invalidated_outside_panes.load(Ordering::SeqCst),
        "the oversized candidate was invalidated under Panes"
    );
    let (out, _rx) = crate::serve::capture(ConnId(1_125));
    assert!(
        oversized.begin_pane_replay(ConnId(1_125), out).is_none(),
        "the rejected oversized host remained replayable"
    );
}

#[test]
fn completed_byte_cap_evicts_oldest_until_the_exact_sum_fits() {
    const HUNDRED_MIB: usize = 100 * 1024 * 1024;
    let w = Wired::new("handler-pane-completed-byte-cap");
    let now = std::time::Instant::now();
    w.fx.handle.set_pane_clock_for_test(Arc::new(move || now));

    let oldest = drained_zero_output_pane(&w, "bytes-0");
    w.fx.handle
        .completed_pane(&id("bytes-0"), &oldest, HUNDRED_MIB);
    let middle = drained_zero_output_pane(&w, "bytes-1");
    w.fx.handle
        .completed_pane(&id("bytes-1"), &middle, HUNDRED_MIB);
    assert_eq!(w.fx.handle.completed_usage_for_test(), (2, 2 * HUNDRED_MIB));

    let invalidated_outside_panes = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&invalidated_outside_panes);
    let handle = Arc::clone(&w.fx.handle);
    oldest.set_invalidation_hook(Box::new(move || {
        observed.store(handle.panes.try_lock().is_ok(), Ordering::SeqCst);
    }));
    let newest = drained_zero_output_pane(&w, "bytes-2");
    w.fx.handle
        .completed_pane(&id("bytes-2"), &newest, HUNDRED_MIB);

    assert_eq!(
        w.fx.handle.completed_usage_for_test(),
        (2, 2 * HUNDRED_MIB),
        "one oldest 100MiB entry is exactly enough to restore the 256MiB cap"
    );
    let panes = lock(&w.fx.handle.panes);
    assert!(!panes.hosts.contains_key(&id("bytes-0")));
    assert!(panes.hosts.contains_key(&id("bytes-1")));
    assert!(panes.hosts.contains_key(&id("bytes-2")));
    drop(panes);
    assert!(
        invalidated_outside_panes.load(Ordering::SeqCst),
        "the byte-cap victim was invalidated under Panes"
    );
}

#[test]
fn completed_order_exhaustion_rejects_only_the_new_candidate() {
    let w = Wired::new("handler-pane-completed-order-exhaustion");
    let existing = drained_zero_output_pane(&w, "order-existing");
    w.fx.handle
        .completed_pane(&id("order-existing"), &existing, 1024);
    {
        let mut panes = lock(&w.fx.handle.panes);
        panes.next_completed_order = u64::MAX;
    }

    let candidate = drained_zero_output_pane(&w, "order-candidate");
    let invalidated_outside_panes = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&invalidated_outside_panes);
    let handle = Arc::clone(&w.fx.handle);
    candidate.set_invalidation_hook(Box::new(move || {
        observed.store(handle.panes.try_lock().is_ok(), Ordering::SeqCst);
    }));
    w.fx.handle
        .completed_pane(&id("order-candidate"), &candidate, 2048);

    assert_eq!(w.fx.handle.completed_usage_for_test(), (1, 1024));
    assert_eq!(w.fx.handle.next_completed_order_for_test(), u64::MAX);
    let panes = lock(&w.fx.handle.panes);
    assert!(matches!(
        panes.hosts.get(&id("order-existing")),
        Some(PaneEntry::Completed { host, .. }) if Arc::ptr_eq(host, &existing)
    ));
    assert!(!panes.hosts.contains_key(&id("order-candidate")));
    drop(panes);
    assert!(
        invalidated_outside_panes.load(Ordering::SeqCst),
        "the refused order-exhausted candidate was invalidated under Panes"
    );
}

/// A failed close belongs to one host generation, not to the agent id forever. The production
/// change that must make this fail is routing failure through id-only `forget_pane`: a stale
/// old host then removes and invalidates the replacement that already owns the same id.
#[test]
fn a_stale_failed_close_cannot_remove_a_replacement_pane() {
    let w = Wired::new("handler-pane-stale-close-failure");
    let old = pane(&w, "root", "sleep 30");
    let (progress, _progress_rx) = std::sync::mpsc::channel();
    let owner = NodeOwner {
        handle: Arc::clone(&w.fx.handle),
        task_id: None,
        repo: w.dir.clone(),
        tx: progress,
        identified: Mutex::new(None),
        announce_to: None,
        owes: Default::default(),
    };
    <NodeOwner as crate::root::PaneOwner>::closing(&owner, &id("root"), &old);
    let replacement = pane(&w, "root", "sleep 30");

    <NodeOwner as crate::root::PaneOwner>::failed(&owner, &id("root"), &old);

    let panes = lock(&w.fx.handle.panes);
    let current = panes.hosts.get(&id("root")).expect("replacement remains");
    assert!(Arc::ptr_eq(current.host(), &replacement));
    drop(panes);
    let conn = ConnId(1_107);
    let (out, _captured) = crate::serve::capture(conn);
    assert!(
        old.begin_pane_replay(conn, out).is_none(),
        "the failed old generation remains replayable"
    );

    w.fx.handle.forget_pane(&id("root"));
    old.shutdown().unwrap();
    replacement.shutdown().unwrap();
}

#[test]
fn stale_generation_callbacks_cannot_change_completed_accounting_or_replacement() {
    let w = Wired::new("handler-pane-stale-completed-accounting");
    let old = pane(&w, "root", "printf old");
    assert!(until(|| old.poll_exited_unreaped().unwrap()));
    w.fx.handle.closing_pane(&id("root"), &old);
    old.shutdown().unwrap();
    let old_charge = old.completed_replay_charge().unwrap();
    w.fx.handle.completed_pane(&id("root"), &old, old_charge);
    assert_eq!(w.fx.handle.completed_usage_for_test(), (1, old_charge));
    assert_eq!(w.fx.handle.next_completed_order_for_test(), 1);

    let replacement = pane(&w, "root", "sleep 30");
    assert_eq!(w.fx.handle.completed_usage_for_test(), (0, 0));
    let order_after_replacement = w.fx.handle.next_completed_order_for_test();

    w.fx.handle.completed_pane(&id("root"), &old, old_charge);
    w.fx.handle.failed_pane(&id("root"), &old);

    assert_eq!(w.fx.handle.completed_usage_for_test(), (0, 0));
    assert_eq!(
        w.fx.handle.next_completed_order_for_test(),
        order_after_replacement,
        "a stale completion must not consume cache order"
    );
    let panes = lock(&w.fx.handle.panes);
    let current = panes.hosts.get(&id("root")).expect("replacement remains");
    assert!(matches!(current, PaneEntry::Live(host) if Arc::ptr_eq(host, &replacement)));
    drop(panes);

    w.fx.handle.forget_pane(&id("root"));
    replacement.shutdown().unwrap();
}

/// A handler clone is not permission to cross the close boundary. The production change that
/// must make this fail is checking `Panes::Live` only before cloning the host and lease: a
/// paused resize can then resume after shutdown, append `r` after cast `x`, mutate the master,
/// and poison an otherwise eligible completed replay with a post-End retention error.
#[test]
fn a_resize_cloned_before_close_cannot_run_after_shutdown() {
    let w = Wired::new("handler-pane-stale-resize");
    let host = pane(&w, "root", "sleep 30");
    let conn = ConnId(1_108);
    let (out, _captured) = crate::serve::capture(conn);
    assert!(w.fx.handle.attach_pane(&id("root"), &out).unwrap().writable);

    let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    *lock(&w.fx.handle.pane_delivery_hook) = Some(Box::new(move || {
        reached_tx.send(()).expect("the assertion side is alive");
        release_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the stale delivery was not released");
    }));
    let handle = Arc::clone(&w.fx.handle);
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let delivery = std::thread::spawn(move || {
        handle.deliver_input(
            conn,
            &marion_core::proto::Input::NodeResize {
                agent_id: id("root"),
                cols: 140,
                rows: 50,
            },
        );
        done_tx.send(()).expect("the assertion side is alive");
    });
    reached_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("delivery did not reach the post-clone seam");

    w.fx.handle.closing_pane(&id("root"), &host);
    host.shutdown().expect("legacy shutdown succeeds");
    let charge_before = host
        .completed_replay_charge()
        .expect("the drained replay is eligible");
    w.fx.handle
        .completed_pane(&id("root"), &host, charge_before);
    release_tx.send(()).expect("the delivery thread is alive");
    done_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the stale delivery did not return");
    delivery.join().unwrap();

    assert_eq!(
        host.master().size().unwrap(),
        crate::pty::WinSize::new(80, 24)
    );
    assert_eq!(
        host.completed_replay_charge().unwrap(),
        charge_before,
        "late control cannot invalidate cached completion eligibility"
    );
    let cast = std::fs::read_to_string(w.dir.join("root.cast")).unwrap();
    let codes = cast
        .lines()
        .skip(1)
        .map(|line| {
            serde_json::from_str::<(f64, String, String)>(line)
                .unwrap()
                .1
        })
        .collect::<Vec<_>>();
    assert_eq!(codes.last().map(String::as_str), Some("x"), "{codes:?}");
    assert!(!codes.iter().any(|code| code == "r"), "{codes:?}");
}

/// Keystrokes obey the same close boundary as resize. The production change that must make
/// this fail is omitting the host gate from `write_input`: a handler clone paused before that
/// call can resume after shutdown and append an `i` record after the cast's terminal `x`.
#[test]
fn input_cloned_before_close_cannot_append_after_shutdown() {
    let w = Wired::new("handler-pane-stale-input");
    let host = pane(&w, "root", "sleep 30");
    let conn = ConnId(1_109);
    let (out, _captured) = crate::serve::capture(conn);
    assert!(w.fx.handle.attach_pane(&id("root"), &out).unwrap().writable);

    let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    *lock(&w.fx.handle.pane_delivery_hook) = Some(Box::new(move || {
        reached_tx.send(()).expect("the assertion side is alive");
        release_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the stale delivery was not released");
    }));
    let handle = Arc::clone(&w.fx.handle);
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let delivery = std::thread::spawn(move || {
        handle.deliver_input(
            conn,
            &marion_core::proto::Input::NodePtyWrite {
                agent_id: id("root"),
                bytes: "late-input".into(),
            },
        );
        done_tx.send(()).expect("the assertion side is alive");
    });
    reached_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("delivery did not reach the post-clone seam");

    w.fx.handle.closing_pane(&id("root"), &host);
    host.shutdown().expect("legacy shutdown succeeds");
    let charge_before = host
        .completed_replay_charge()
        .expect("the drained replay is eligible");
    w.fx.handle
        .completed_pane(&id("root"), &host, charge_before);
    release_tx.send(()).expect("the delivery thread is alive");
    done_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the stale delivery did not return");
    delivery.join().unwrap();

    assert_eq!(host.completed_replay_charge().unwrap(), charge_before);
    let cast = std::fs::read_to_string(w.dir.join("root.cast")).unwrap();
    let codes = cast
        .lines()
        .skip(1)
        .map(|line| {
            serde_json::from_str::<(f64, String, String)>(line)
                .unwrap()
                .1
        })
        .collect::<Vec<_>>();
    assert_eq!(codes.last().map(String::as_str), Some("x"), "{codes:?}");
    assert!(!codes.iter().any(|code| code == "i"), "{codes:?}");
}

/// Sealing is a nonblocking registry transition; draining is shutdown's host-local barrier.
/// The production change that must make this fail is waiting for admitted control while the
/// global `Panes` lock is held, or writing cast `x` before that admitted mutation completes.
#[test]
fn closing_releases_the_registry_before_shutdown_drains_admitted_control() {
    let w = Wired::new("handler-pane-control-drain");
    let host = pane(&w, "root", "sleep 30");
    let conn = ConnId(1_110);
    let (out, _captured) = crate::serve::capture(conn);
    assert!(w.fx.handle.attach_pane(&id("root"), &out).unwrap().writable);

    let (admitted_tx, admitted_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    host.set_post_cast_input_hook(Box::new(move || {
        admitted_tx.send(()).expect("the assertion side is alive");
        release_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the admitted control was not released");
    }));
    let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
    host.set_input_delivery_wait_signal(waiting_tx);
    let handle = Arc::clone(&w.fx.handle);
    let (delivery_done_tx, delivery_done_rx) = std::sync::mpsc::sync_channel(1);
    let delivery = std::thread::spawn(move || {
        handle.deliver_input(
            conn,
            &marion_core::proto::Input::NodePtyWrite {
                agent_id: id("root"),
                bytes: "admitted".into(),
            },
        );
        delivery_done_tx
            .send(())
            .expect("the assertion side is alive");
    });
    admitted_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("input was not admitted at the host boundary");

    let closing_handle = Arc::clone(&w.fx.handle);
    let closing_host = Arc::clone(&host);
    let (closing_done_tx, closing_done_rx) = std::sync::mpsc::sync_channel(1);
    let closing = std::thread::spawn(move || {
        closing_handle.closing_pane(&id("root"), &closing_host);
        closing_done_tx
            .send(())
            .expect("the assertion side is alive");
    });

    let shutdown_host = Arc::clone(&host);
    let (shutdown_done_tx, shutdown_done_rx) = std::sync::mpsc::sync_channel(1);
    let shutdown = std::thread::spawn(move || {
        shutdown_done_tx
            .send(shutdown_host.shutdown())
            .expect("the assertion side is alive");
    });
    let registry = Arc::clone(&w.fx.handle);
    let (lookup_done_tx, lookup_done_rx) = std::sync::mpsc::sync_channel(1);
    let lookup = std::thread::spawn(move || {
        lookup_done_tx
            .send(registry.panes())
            .expect("the assertion side is alive");
    });

    waiting_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("shutdown did not observe the unresolved input delivery");
    closing_done_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("Closing waited for admitted control");
    lookup_done_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("Closing held the global pane registry while control was parked");
    host.fail_next_master_input("injected delivery failure after shutdown observed the barrier");
    release_tx.send(()).expect("the delivery thread is alive");

    delivery_done_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("admitted input did not finish");
    let shutdown_result = shutdown_done_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("shutdown did not finish after control drained");
    shutdown_result.expect("legacy shutdown succeeds");
    closing.join().unwrap();
    lookup.join().unwrap();
    delivery.join().unwrap();
    shutdown.join().unwrap();

    assert!(
        host.completed_replay_charge().is_err(),
        "a cast-recorded input that failed delivery remained cache-eligible"
    );
    let cast = std::fs::read_to_string(w.dir.join("root.cast")).unwrap();
    let codes = cast
        .lines()
        .skip(1)
        .map(|line| {
            serde_json::from_str::<(f64, String, String)>(line)
                .unwrap()
                .1
        })
        .collect::<Vec<_>>();
    assert_eq!(codes.last().map(String::as_str), Some("x"), "{codes:?}");
    assert!(codes.iter().any(|code| code == "i"), "{codes:?}");
}

/// A cast-recorded input remains part of the completion decision until its master write has
/// either succeeded or failed. A permit that ends at the cast record lets shutdown cache an
/// eligible replay while the write is still parked; its later failure then arrives too late to
/// retract the registry's Completed entry.
#[test]
fn unresolved_input_delivery_cannot_be_published_as_completed() {
    let w = Wired::new("handler-pane-input-delivery-completion");
    let host = pane(&w, "root", "sleep 30");
    let conn = ConnId(1_116);
    let (out, _captured) = crate::serve::capture(conn);
    assert!(w.fx.handle.attach_pane(&id("root"), &out).unwrap().writable);

    let (recorded_tx, recorded_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    host.set_post_cast_input_hook(Box::new(move || {
        recorded_tx.send(()).expect("the assertion side is alive");
        release_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the parked input delivery was not released");
    }));

    let handle = Arc::clone(&w.fx.handle);
    let delivery = std::thread::spawn(move || {
        handle.deliver_input(
            conn,
            &marion_core::proto::Input::NodePtyWrite {
                agent_id: id("root"),
                bytes: "recorded-but-undelivered".into(),
            },
        );
    });
    recorded_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("input never reached the post-cast delivery seam");

    w.fx.handle.closing_pane(&id("root"), &host);
    let shutdown_host = Arc::clone(&host);
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::sync_channel(1);
    let shutdown = std::thread::spawn(move || {
        shutdown_tx
            .send(shutdown_host.shutdown())
            .expect("the assertion side is alive");
    });
    let shutdown_result = shutdown_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("bounded shutdown waited indefinitely for an input write");
    shutdown_result.expect("legacy shutdown remains successful");
    let charge_before_release = host.completed_replay_charge();
    if let Ok(charge) = charge_before_release {
        w.fx.handle.completed_pane(&id("root"), &host, charge);
    } else {
        w.fx.handle.failed_pane(&id("root"), &host);
    }

    release_tx.send(()).expect("the delivery thread is alive");
    delivery.join().unwrap();
    shutdown.join().unwrap();

    assert!(
        charge_before_release.is_err(),
        "shutdown qualified replay while an input delivery outcome was unresolved"
    );
    assert_eq!(
        w.fx.handle.panes(),
        0,
        "an unresolved delivery was published into the completed-pane cache"
    );
    let (out, _rx) = crate::serve::capture(ConnId(1_117));
    assert!(
        host.begin_pane_replay(ConnId(1_117), out).is_none(),
        "the failed host remained available for internal replay"
    );
    let cast = std::fs::read_to_string(w.dir.join("root.cast")).unwrap();
    let codes = cast
        .lines()
        .skip(1)
        .map(|line| {
            serde_json::from_str::<(f64, String, String)>(line)
                .unwrap()
                .1
        })
        .collect::<Vec<_>>();
    assert_eq!(codes.last().map(String::as_str), Some("x"), "{codes:?}");
    assert!(codes.iter().any(|code| code == "i"), "{codes:?}");
}

/// Legacy pane presence means a live, attachable terminal—not retained replay state. The
/// production change that must make this fail is projecting every `Panes` key: Closing and
/// Completed then advertise `pane: true` while legacy attach returns no pane or listener.
#[test]
fn only_live_panes_are_visible_on_legacy_projection_and_attach() {
    let w = Wired::new("handler-pane-live-projection");
    let host = pane(&w, "root", "sleep 30");
    say(&events_of(&w.fx, "root"), "root", &["ready"]);
    assert!(w.fx.handle.node_get(&id("root"), None).unwrap().node.pane);
    assert!(w.fx.handle.pane_ids().contains(&id("root")));

    w.fx.handle.closing_pane(&id("root"), &host);
    assert!(!w.fx.handle.node_get(&id("root"), None).unwrap().node.pane);
    assert!(!w.fx.handle.pane_ids().contains(&id("root")));
    let (closing_out, _closing_rx) = crate::serve::capture(ConnId(1_111));
    let closing =
        w.fx.handle
            .node_attach(&id("root"), false, &closing_out)
            .unwrap();
    assert!(!closing.node.pane);
    assert!(closing.pane.is_none());
    assert_eq!(host.listeners(), 0, "Closing installed a dead listener");

    let closing_v1_conn = ConnId(1_204);
    let (closing_v1_out, closing_v1_rx) = crate::serve::capture(closing_v1_conn);
    let closing_v1 =
        w.fx.handle
            .node_attach(&id("root"), true, &closing_v1_out)
            .expect("Closing remains explicitly replayable");
    let closing_v1_pane = closing_v1.pane.expect("Closing advertises only v1 replay");
    assert!(
        closing_v1.node.pane,
        "attach summary describes this exact v1 pane"
    );
    assert!(!closing_v1_pane.writable);
    assert_eq!(closing_v1_pane.held_by, None);
    assert!(
        closing_v1_pane.ended,
        "a Closing pane is read-only because it ended, not because a writer holds it"
    );
    let closing_descriptor = closing_v1_pane
        .pane_ready
        .expect("Closing advertises a response-first cursor");
    let before_ready = closing_v1_rx
        .try_iter()
        .map(|line| Frame::from_line(std::str::from_utf8(&line).unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert!(before_ready.iter().all(|frame| {
        matches!(frame, Frame::Notification(note) if matches!(note.event, Event::NodeEvent { .. }))
    }));

    host.shutdown().expect("legacy shutdown succeeds");
    let charge = host.completed_replay_charge().expect("replay is eligible");
    w.fx.handle.completed_pane(&id("root"), &host, charge);
    w.fx.handle.input(
        closing_v1_conn,
        &marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
            agent_id: id("root"),
            token: closing_descriptor.token,
            cut: closing_descriptor.cut,
        }),
    );
    let pending_frames = closing_v1_rx
        .try_iter()
        .map(|line| Frame::from_line(std::str::from_utf8(&line).unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert!(matches!(
        pending_frames.first(),
        Some(Frame::Notification(note))
            if matches!(&note.event, Event::NodePaneFrame(frame)
                if frame.seq == 0
                    && matches!(frame.frame, marion_core::proto::PaneFrameKindV1::Resize { cols: 80, rows: 24 }))
    ));
    assert!(matches!(
        pending_frames.last(),
        Some(Frame::Notification(note))
            if matches!(&note.event, Event::NodePaneFrame(frame)
                if matches!(frame.frame, marion_core::proto::PaneFrameKindV1::End {}))
    ));
    assert!(!w.fx.handle.node_get(&id("root"), None).unwrap().node.pane);
    assert!(!w.fx.handle.pane_ids().contains(&id("root")));
    let (completed_out, _completed_rx) = crate::serve::capture(ConnId(1_112));
    let completed =
        w.fx.handle
            .node_attach(&id("root"), false, &completed_out)
            .unwrap();
    assert!(!completed.node.pane);
    assert!(completed.pane.is_none());
    assert_eq!(host.listeners(), 0, "Completed installed a dead listener");

    let (completed_v1_out, _completed_v1_rx) = crate::serve::capture(ConnId(1_205));
    let completed_v1 =
        w.fx.handle
            .node_attach(&id("root"), true, &completed_v1_out)
            .expect("Completed remains explicitly replayable");
    let completed_v1_pane = completed_v1
        .pane
        .expect("Completed advertises only v1 replay");
    assert!(completed_v1.node.pane);
    assert!(!completed_v1_pane.writable);
    assert_eq!(completed_v1_pane.held_by, None);
    assert!(
        completed_v1_pane.ended,
        "a Completed pane is read-only because it ended"
    );
    assert!(completed_v1_pane.pane_ready.is_some());
    host.unlisten(closing_v1_conn);
    host.unlisten(ConnId(1_205));
}

/// A cursor activated while Live remains registered through the same host's Closing and
/// Completed transitions, and receives the one retained terminal End.
#[test]
fn ready_pane_v1_subscription_survives_same_host_completion() {
    let w = Wired::new("handler-pane-v1-ready-completion");
    let host = pane(&w, "root", "sleep 30");
    say(&events_of(&w.fx, "root"), "root", &["before-ready"]);
    let conn = ConnId(1_206);
    let (out, captured) = crate::serve::capture(conn);
    let attached =
        w.fx.handle
            .node_attach(&id("root"), true, &out)
            .expect("Live v1 attach");
    let descriptor = attached
        .pane
        .unwrap()
        .pane_ready
        .expect("Live v1 descriptor");
    let before_ready = captured
        .try_iter()
        .map(|line| Frame::from_line(std::str::from_utf8(&line).unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert!(before_ready.iter().all(|frame| {
        matches!(frame, Frame::Notification(note) if matches!(note.event, Event::NodeEvent { .. }))
    }));
    w.fx.handle.input(
        conn,
        &marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
            agent_id: id("root"),
            token: descriptor.token,
            cut: descriptor.cut,
        }),
    );
    let initial = captured.try_recv().expect("initial geometry");
    assert!(matches!(
        Frame::from_line(std::str::from_utf8(&initial).unwrap()).unwrap(),
        Frame::Notification(note)
            if matches!(note.event, Event::NodePaneFrame(ref frame)
                if frame.seq == 0
                    && matches!(frame.frame, marion_core::proto::PaneFrameKindV1::Resize { cols: 80, rows: 24 }))
    ));
    assert!(captured.try_iter().next().is_none(), "replay reaches Ready");

    w.fx.handle.closing_pane(&id("root"), &host);
    host.shutdown().unwrap();
    let charge = host.completed_replay_charge().unwrap();
    w.fx.handle.completed_pane(&id("root"), &host, charge);

    let tail = captured
        .try_iter()
        .map(|line| Frame::from_line(std::str::from_utf8(&line).unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert!(matches!(
        tail.last(),
        Some(Frame::Notification(note))
            if matches!(&note.event, Event::NodePaneFrame(frame)
                if matches!(frame.frame, marion_core::proto::PaneFrameKindV1::End {}))
    ));
    assert_eq!(out.departed(), None);
    host.unlisten(conn);
}

#[test]
fn completed_commit_preserves_final_legacy_tail_then_clears_listener() {
    let w = Wired::new("handler-pane-completed-legacy-tail");
    let host = pane(&w, "root", "sleep 0.05; printf 'final-tail'");
    let conn = ConnId(1_126);
    let (out, rx) = crate::serve::capture(conn);
    host.listen(out);
    let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    let first = AtomicBool::new(true);
    host.set_legacy_delivery_hook(Box::new(move || {
        if first.swap(false, Ordering::SeqCst) {
            reached_tx.send(()).expect("the assertion side is alive");
            release_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("the final legacy delivery was not released");
        }
    }));
    reached_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the final tail never reserved its legacy delivery");

    w.fx.handle.closing_pane(&id("root"), &host);
    assert_eq!(
        host.listeners(),
        1,
        "Closing cleared the listener before the reader drained its final tail"
    );
    release_tx.send(()).unwrap();
    assert!(until(|| host.bytes_read() >= b"final-tail".len() as u64));
    host.shutdown().unwrap();
    let mut tail = String::new();
    assert!(
        until(|| {
            tail.extend(rx.try_iter().filter_map(|line| {
                let frame = Frame::from_line(std::str::from_utf8(&line).unwrap()).unwrap();
                match frame {
                    Frame::Notification(note) => match note.event {
                        Event::NodePty { bytes, .. } => Some(bytes),
                        _ => None,
                    },
                    _ => None,
                }
            }));
            tail == "final-tail"
        }),
        "shutdown joined before the final legacy tail was observable: {tail:?}"
    );

    let charge = host.completed_replay_charge().unwrap();
    w.fx.handle.completed_pane(&id("root"), &host, charge);
    assert_eq!(host.listeners(), 0, "Completed retained a legacy listener");

    // `emit_for_test` delivers synchronously; an empty queue immediately afterwards is the
    // causal refusal, with no scheduler delay standing in for correctness.
    host.emit_for_test("post-completion");
    assert!(
        rx.try_recv().is_err(),
        "a completed pane kept delivering legacy NodePty frames"
    );
}

/// The response's `node.pane` bit describes the exact attach committed by this call, not a
/// registry snapshot from before event replay. The production change that must make this fail
/// is summarizing first and selecting the pane later: Closing in that seam yields
/// `node.pane=true` alongside `pane=None`.
#[test]
fn node_attach_summary_matches_the_post_replay_pane_selection() {
    let w = Wired::new("handler-pane-attach-selection");
    let host = pane(&w, "root", "sleep 30");
    say(&events_of(&w.fx, "root"), "root", &["before-selection"]);
    let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    *lock(&w.fx.handle.pane_attach_selection_hook) = Some(Box::new(move || {
        reached_tx.send(()).expect("the assertion side is alive");
        release_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("attach selection was not released");
    }));
    let conn = ConnId(1_117);
    let (out, captured) = crate::serve::capture(conn);
    let handle = Arc::clone(&w.fx.handle);
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    let attaching = std::thread::spawn(move || {
        result_tx
            .send(handle.node_attach(&id("root"), false, &out))
            .expect("the assertion side is alive");
    });
    reached_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("event replay did not reach final pane selection");
    w.fx.handle.closing_pane(&id("root"), &host);
    release_tx.send(()).expect("the attach worker is alive");
    let attached = result_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("attach did not finish")
        .expect("node attach succeeds");
    attaching.join().unwrap();

    assert_eq!(
        attached.node.pane,
        attached.pane.is_some(),
        "response summary disagrees with exact pane selection"
    );
    let frames = captured
        .try_iter()
        .map(|bytes| Frame::from_line(std::str::from_utf8(&bytes).unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert!(matches!(
        frames.first(),
        Some(Frame::Notification(note)) if matches!(note.event, Event::NodeEvent { .. })
    ));
    assert_eq!(host.listeners(), 0, "Closing gained a legacy listener");
    host.shutdown().unwrap();
}

/// A versioned attach may not return a descriptor for a host invalidated at the final
/// selection seam. The parked event cursor and write lease are rolled back with the token.
#[test]
fn pane_v1_attach_refuses_a_replaced_generation_at_final_selection() {
    let w = Wired::new("handler-pane-v1-generation-race");
    let old = pane(&w, "root", "sleep 30");
    say(&events_of(&w.fx, "root"), "root", &["before-replacement"]);
    let replacement = unregistered_pane(&w, "root", "sleep 30");
    let handle = Arc::clone(&w.fx.handle);
    *lock(&w.fx.handle.pane_attach_selection_hook) = Some(Box::new(move || {
        handle.register_pane(&id("root"), Arc::clone(&replacement));
    }));
    let conn = ConnId(1_201);
    let (out, _captured) = crate::serve::capture(conn);

    let error =
        w.fx.handle
            .node_attach(&id("root"), true, &out)
            .expect_err("a dead Ready descriptor was returned");

    assert_eq!(error.kind(), Some(FailureKind::Conflict));
    assert_eq!(w.fx.handle.attachments(), 0);
    assert_eq!(old.writer(), None);
    assert_eq!(old.listeners(), 0);
    old.shutdown().unwrap();
    let current = lock(&w.fx.handle.panes)
        .hosts
        .get(&id("root"))
        .unwrap()
        .host()
        .clone();
    current.shutdown().unwrap();
}

/// A live pane whose write half another connection already holds is the *other* reason a v1
/// attach is read-only, and it must not be spelled the same way: `ended` stays false, and
/// `held_by` names the colleague. Without this pair a client cannot tell a node somebody else
/// is typing into from a node that has finished.
#[test]
fn pane_v1_attach_separates_a_busy_writer_from_a_pane_that_ended() {
    let w = Wired::new("handler-pane-v1-busy-vs-ended");
    let host = pane(&w, "root", "sleep 30");
    say(&events_of(&w.fx, "root"), "root", &["running"]);

    let writer_conn = ConnId(1_301);
    let (writer_out, _writer_rx) = crate::serve::capture(writer_conn);
    let first =
        w.fx.handle
            .node_attach(&id("root"), true, &writer_out)
            .expect("the first attach takes the write half");
    let first_pane = first.pane.expect("a live pane answers v1");
    assert!(first_pane.writable);
    assert!(!first_pane.ended);

    let reader_conn = ConnId(1_302);
    let (reader_out, _reader_rx) = crate::serve::capture(reader_conn);
    let second =
        w.fx.handle
            .node_attach(&id("root"), true, &reader_out)
            .expect("a second attach is read-only, not refused");
    let second_pane = second.pane.expect("a live pane answers v1");
    assert!(!second_pane.writable);
    assert_eq!(
        second_pane.held_by,
        Some(writer_conn.0),
        "a busy write half names its holder"
    );
    assert!(
        !second_pane.ended,
        "the node is still running; only its keyboard is taken"
    );

    host.unlisten(writer_conn);
    host.unlisten(reader_conn);
    host.shutdown().unwrap();
}

/// Closing the same host at the final seam preserves its exact replay token but revokes the
/// keyboard lease. The response must describe the lifecycle it actually committed.
#[test]
fn pane_v1_attach_finalizes_same_host_closing_as_read_only() {
    let w = Wired::new("handler-pane-v1-closing-race");
    let host = pane(&w, "root", "sleep 30");
    say(&events_of(&w.fx, "root"), "root", &["before-closing"]);
    let handle = Arc::clone(&w.fx.handle);
    let closing_host = Arc::clone(&host);
    *lock(&w.fx.handle.pane_attach_selection_hook) = Some(Box::new(move || {
        handle.closing_pane(&id("root"), &closing_host);
    }));
    let conn = ConnId(1_203);
    let (out, _captured) = crate::serve::capture(conn);

    let attached =
        w.fx.handle
            .node_attach(&id("root"), true, &out)
            .expect("same-host Closing keeps replay reachable");
    let pane = attached.pane.expect("Closing remains v1 replayable");
    let descriptor = pane.pane_ready.expect("the reserved descriptor survives");

    assert!(!pane.writable);
    assert_eq!(pane.held_by, None);
    assert!(
        pane.ended,
        "a pane that closed between reservation and commit ended; its writer lease was not \
         taken by anybody else, and a claimant told otherwise discards the replay it attached \
         for"
    );
    assert_eq!(host.writer(), None);
    assert!(host.pane_replay_reserved(conn, &descriptor.token, descriptor.cut));
    host.unlisten(conn);
    host.shutdown().unwrap();
}

/// Explicit v1 never downgrades after reservation failure. Even a durable NodeEvent prefix is
/// untouched because reservation occurs before event delivery, cursor parking, or write lease.
#[test]
fn failed_pane_v1_reservation_has_no_attach_side_effects() {
    let w = Wired::new("handler-pane-v1-reservation-refusal");
    let host = pane(&w, "root", "sleep 30");
    say(&events_of(&w.fx, "root"), "root", &["must-not-deliver"]);
    host.invalidate_pane_streams();
    let conn = ConnId(1_207);
    let (out, captured) = crate::serve::capture(conn);

    let error =
        w.fx.handle
            .node_attach(&id("root"), true, &out)
            .expect_err("invalid replay silently downgraded");

    assert_eq!(error.kind(), Some(FailureKind::Internal));
    assert_eq!(w.fx.handle.attachments(), 0);
    assert_eq!(host.writer(), None);
    assert_eq!(host.listeners(), 0);
    assert!(captured.try_iter().next().is_none());
    host.shutdown().unwrap();
}

/// Same-id registration is a generation replacement, not a map overwrite. The production
/// change that must make this fail is leaving old leases/listeners/replay valid, or invalidating
/// the old host while `Panes` is locked: the stale lease can resize the new terminal and a
/// blocking last-host drop can freeze every pane operation.
#[test]
fn replacing_a_pane_revokes_the_old_generation_outside_the_registry_lock() {
    let w = Wired::new("handler-pane-replacement");
    let old = pane(&w, "root", "printf 'old-prefix'; sleep 30");
    assert!(until(|| old.bytes_read() >= b"old-prefix".len() as u64));
    let conn = ConnId(1_113);
    let (out, _legacy_rx) = crate::serve::capture(conn);
    assert!(w.fx.handle.attach_pane(&id("root"), &out).unwrap().writable);
    let replay_conn = ConnId(1_114);
    let (replay_out, replay_rx) = crate::serve::capture(replay_conn);
    let descriptor = old
        .begin_pane_replay(replay_conn, replay_out.clone())
        .expect("old replay is reserved");

    let handle = Arc::downgrade(&w.fx.handle);
    let (invalidated_tx, invalidated_rx) = std::sync::mpsc::sync_channel(1);
    old.set_invalidation_hook(Box::new(move || {
        let handle = handle.upgrade().expect("the registry is alive");
        invalidated_tx
            .send(handle.panes.try_lock().is_ok())
            .expect("the assertion side is alive");
    }));
    let replacement = pane(&w, "root", "sleep 30");
    assert!(
        invalidated_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("replacement did not invalidate the old generation"),
        "old generation was invalidated while Panes was locked"
    );

    w.fx.handle.deliver_input(
        conn,
        &marion_core::proto::Input::NodeResize {
            agent_id: id("root"),
            cols: 140,
            rows: 50,
        },
    );
    assert_eq!(
        replacement.master().size().unwrap(),
        crate::pty::WinSize::new(80, 24),
        "the old lease resized the replacement"
    );
    assert_eq!(
        old.listeners(),
        0,
        "old legacy listener survived replacement"
    );
    assert_eq!(old.writer(), None, "old writer lease survived replacement");
    old.pane_ready(replay_conn, &descriptor.token, descriptor.cut);
    assert!(
        replay_rx.try_iter().next().is_none(),
        "old retained replay activated after replacement"
    );
    assert!(old.begin_pane_replay(replay_conn, replay_out).is_none());

    let attached = w.fx.handle.attach_pane(&id("root"), &out).unwrap();
    assert!(attached.writable, "replacement did not issue a fresh lease");
    assert_eq!(replacement.writer(), Some(conn));
    w.fx.handle.forget_pane(&id("root"));
    old.shutdown().unwrap();
    replacement.shutdown().unwrap();
}

/// Replacement has one strict legacy cut: no old NodePty delivery can emerge after the new
/// Live generation is visible. The production change that must make this fail is publishing
/// the map swap before synchronizing old listener delivery, or allowing same-Arc lifecycle
/// regression from Closing/Completed back to Live.
#[test]
fn replacement_cuts_old_legacy_delivery_before_publishing_new_live_generation() {
    let w = Wired::new("handler-pane-replacement-cutoff");
    let old = pane(&w, "root", "sleep 30");
    let (old_out, old_rx) = crate::serve::capture(ConnId(1_115));
    old.listen(old_out);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    old.set_legacy_delivery_hook(Box::new(move || {
        entered_tx.send(()).expect("the assertion side is alive");
        release_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("old delivery was not released");
    }));
    let emitting_old = Arc::clone(&old);
    let emit = std::thread::spawn(move || emitting_old.emit_for_test("old-after-cut"));
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("old delivery did not reach the cutoff seam");

    let replacement = unregistered_pane(&w, "root", "sleep 30");
    let registering = Arc::clone(&w.fx.handle);
    let registering_host = Arc::clone(&replacement);
    let (registered_tx, registered_rx) = std::sync::mpsc::sync_channel(1);
    let register = std::thread::spawn(move || {
        registering.register_pane(&id("root"), registering_host);
        registered_tx.send(()).expect("the assertion side is alive");
    });
    assert!(
        until(|| {
            matches!(
                lock(&w.fx.handle.panes).hosts.get(&id("root")),
                Some(PaneEntry::Replacing(current)) if Arc::ptr_eq(current, &replacement)
            )
        }),
        "replacement did not enter its non-live cutoff phase"
    );
    assert!(
        !lock(&w.fx.handle.panes).has_live(&id("root")),
        "replacement became Live before old delivery was cut"
    );
    release_tx.send(()).expect("the emitter is alive");
    emit.join().unwrap();
    registered_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("replacement was not published after cutoff");
    register.join().unwrap();
    assert!(
        lock(&w.fx.handle.panes)
            .hosts
            .get(&id("root"))
            .is_some_and(|entry| Arc::ptr_eq(entry.host(), &replacement)),
        "replacement was not published"
    );
    let before_cut = old_rx.try_iter().collect::<Vec<_>>();
    assert_eq!(
        before_cut.len(),
        1,
        "the one delivery reserved before cutoff completes before publication"
    );
    old.emit_for_test("old-definitely-after-cut");
    assert!(
        old_rx.try_iter().next().is_none(),
        "old NodePty was admitted after replacement visibility"
    );

    w.fx.handle.closing_pane(&id("root"), &replacement);
    w.fx.handle
        .register_pane(&id("root"), Arc::clone(&replacement));
    assert!(
        !lock(&w.fx.handle.panes).has_live(&id("root")),
        "same Arc resurrected Closing as Live"
    );

    let (forgotten_out, _forgotten_rx) = crate::serve::capture(ConnId(1_116));
    replacement.listen(forgotten_out);
    w.fx.handle.failed_pane(&id("root"), &replacement);
    assert_eq!(
        replacement.listeners(),
        0,
        "failed cleanup retained listeners"
    );
    old.shutdown().unwrap();
    replacement.shutdown().unwrap();
}

/// A replacement child may exit while publication is waiting for the old generation's
/// admitted legacy delivery. Closing must wait for that publication transaction and then seal
/// the new host; returning early from `Replacing(new)` would resurrect the dead child as Live.
#[test]
fn closing_a_replacement_waits_for_its_generation_to_publish() {
    let w = Wired::new("handler-pane-replacement-close-race");
    let old = pane(&w, "root", "sleep 30");
    let (old_out, _old_rx) = crate::serve::capture(ConnId(1_208));
    old.listen(old_out);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    old.set_legacy_delivery_hook(Box::new(move || {
        entered_tx.send(()).expect("the assertion side is alive");
        release_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("old delivery was not released");
    }));
    let emitting = Arc::clone(&old);
    let emit = std::thread::spawn(move || emitting.emit_for_test("reserved"));
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("old delivery did not enter");

    let replacement = unregistered_pane(&w, "root", "sleep 30");
    let registering = Arc::clone(&w.fx.handle);
    let registering_host = Arc::clone(&replacement);
    let register = std::thread::spawn(move || {
        registering.register_pane(&id("root"), registering_host);
    });
    assert!(until(|| {
        matches!(
            lock(&w.fx.handle.panes).hosts.get(&id("root")),
            Some(PaneEntry::Replacing(current)) if Arc::ptr_eq(current, &replacement)
        )
    }));

    let closing = Arc::clone(&w.fx.handle);
    let closing_host = Arc::clone(&replacement);
    let (closed_tx, closed_rx) = std::sync::mpsc::sync_channel(1);
    let close = std::thread::spawn(move || {
        closing.closing_pane(&id("root"), &closing_host);
        closed_tx.send(()).expect("the assertion side is alive");
    });
    assert!(
        closed_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "Closing returned while the replacement transaction was still Replacing"
    );
    release_tx.send(()).unwrap();
    emit.join().unwrap();
    register.join().unwrap();
    closed_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("Closing did not follow publication");
    close.join().unwrap();

    assert!(matches!(
        lock(&w.fx.handle.panes).hosts.get(&id("root")),
        Some(PaneEntry::Closing(current)) if Arc::ptr_eq(current, &replacement)
    ));
    old.shutdown().unwrap();
    replacement.shutdown().unwrap();
}

/// Dark wire behavior cannot accidentally depend on the internal replay engine being enabled.
/// Legacy live attach remains available and advertises no cursor even after replay invalidation.
#[test]
fn dark_pane_attach_does_not_consult_internal_replay_state() {
    let w = Wired::new("handler-pane-dark-internal-state");
    let host = pane(&w, "root", "sleep 30");
    host.invalidate_pane_streams();
    let conn = ConnId(104);
    let (out, rx) = crate::serve::capture(conn);
    let attached =
        w.fx.handle
            .node_attach(&id("root"), false, &out)
            .expect("legacy attach does not consult replay state");
    let pane = attached.pane.expect("the pty remains attachable");

    assert!(pane.pane_ready.is_none());
    assert_eq!(host.listeners(), 1);
    assert_eq!(host.writer(), Some(conn));
    assert!(
        rx.try_iter().next().is_none(),
        "dark attach cannot emit replay or pane frames"
    );
    host.unlisten(conn);
    host.shutdown().unwrap();
}

/// Legacy attach has always replayed the durable node stream before making live PTY bytes
/// observable. Force a PTY emit at the listener-install seam so the queue order, not timing,
/// proves that contract.
#[test]
fn legacy_attach_enqueues_replayed_node_events_before_concurrent_pty_output() {
    let w = Wired::new("handler-pane-legacy-order");
    say(&events_of(&w.fx, "root"), "root", &["event-first"]);
    let host = pane(&w, "root", "sleep 30");
    let conn = ConnId(106);
    let (out, captured) = crate::serve::capture(conn);
    let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    *lock(&w.fx.handle.pane_listener_hook) = Some(Box::new(move || {
        reached_tx
            .send(())
            .expect("the deterministic assertion side is alive");
        release_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the deterministic listener hook was not released");
    }));

    std::thread::scope(|scope| {
        let handle = Arc::clone(&w.fx.handle);
        let out = out.clone();
        let attaching = scope.spawn(move || handle.node_attach(&id("root"), false, &out));
        reached_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("attach reached the listener-install seam");
        host.emit_for_test("pty-second");
        release_tx.send(()).expect("the attach worker is alive");
        attaching.join().unwrap().unwrap();
    });
    let order = captured
        .try_iter()
        .map(|bytes| Frame::from_line(std::str::from_utf8(&bytes).unwrap()).unwrap())
        .filter_map(|frame| match frame {
            Frame::Notification(note) => Some(match note.event {
                Event::NodeEvent { .. } => "event",
                Event::NodePty { .. } => "pty",
                _ => "other",
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    host.unlisten(conn);
    host.shutdown().unwrap();

    assert_eq!(order, ["event", "pty"]);
}

/// Explicit pane-v1 reserves a response-first replay and does not install the legacy listener.
/// No pane frame may precede the response; the exact advertised Ready activates sequence zero.
#[test]
fn pane_stream_capability_reserves_response_first_replay() {
    let w = Wired::new("handler-pane-wire-active");
    let host = pane(&w, "root", "printf 'prefix'; sleep 30");
    assert!(
        until(|| host.bytes_read() >= b"prefix".len() as u64),
        "the retained prefix never arrived"
    );
    host.resize(crate::pty::WinSize::new(100, 40)).unwrap();
    let mut c = w.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());

    call(
        &mut c,
        Call::NodeAttach(marion_core::proto::params::NodeAttachParams {
            agent_id: id("root"),
            pane_stream: Some(marion_core::proto::params::PaneStreamCapabilityV1::new()),
        }),
        1,
    );
    let mut before_response = Vec::new();
    let attached = loop {
        match next_frame(&mut r) {
            Frame::Response(response) => break attached_ok(response.outcome),
            Frame::Notification(note) => before_response.push(note.event),
            other => panic!("unexpected frame while attaching: {other:?}"),
        }
    };
    let pane = attached.pane.expect("the real pty is attachable");
    let descriptor = pane
        .pane_ready
        .expect("explicit v1 advertises the reserved replay");
    assert_eq!((pane.cols, pane.rows), (100, 40));
    assert_eq!(host.listeners(), 0, "opt-in installed a legacy listener");
    assert!(
        before_response
            .iter()
            .all(|event| !matches!(event, Event::NodePaneFrame(_))),
        "pane frames preceded the attach response"
    );
    let ready = Frame::Input(marion_core::proto::ClientNotification::new(
        marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
            agent_id: id("root"),
            token: descriptor.token,
            cut: descriptor.cut,
        }),
    ));
    c.write_all(ready.to_line().as_bytes()).unwrap();
    c.flush().unwrap();
    let mut pane_frames = Vec::new();
    while pane_frames.len() < descriptor.cut as usize {
        let Frame::Notification(note) = next_frame(&mut r) else {
            panic!("exact Ready did not activate the replay")
        };
        let Event::NodePaneFrame(frame) = note.event else {
            panic!("Ready emitted a non-pane notification")
        };
        pane_frames.push(frame);
    }
    assert_eq!(
        pane_frames
            .iter()
            .map(|frame| frame.seq)
            .collect::<Vec<_>>(),
        (0..descriptor.cut).collect::<Vec<_>>()
    );
    assert!(matches!(
        pane_frames.first(),
        Some(frame)
            if matches!(frame.frame, marion_core::proto::PaneFrameKindV1::Resize { cols: 80, rows: 24 })
    ));
    let output_at = pane_frames.iter().position(|frame| {
        matches!(&frame.frame, marion_core::proto::PaneFrameKindV1::Output { bytes }
            if bytes.as_bytes() == b"prefix")
    });
    let resize_at = pane_frames.iter().position(|frame| {
        matches!(
            frame.frame,
            marion_core::proto::PaneFrameKindV1::Resize {
                cols: 100,
                rows: 40
            }
        )
    });
    assert!(
        matches!((output_at, resize_at), (Some(output), Some(resize)) if output < resize),
        "historical output/resize order was lost: {pane_frames:?}"
    );
}

#[test]
fn pane_ready_routing_is_exact_and_preserves_the_valid_pending_slot() {
    let w = Wired::new("handler-pane-ready-exact");
    let host = pane(&w, "root", "printf 'prefix'; sleep 30");
    assert!(
        until(|| host.bytes_read() >= b"prefix".len() as u64),
        "the retained prefix never arrived"
    );
    let conn = ConnId(105);
    let (out, captured) = crate::serve::capture(conn);
    let descriptor = host
        .begin_pane_replay(conn, out)
        .expect("the internal replay seam remains testable");

    let wrong = [
        (
            conn,
            id("root"),
            marion_core::proto::PaneReadyTokenV1::new([0xa5; 32]),
            descriptor.cut,
        ),
        (
            conn,
            id("root"),
            descriptor.token.clone(),
            descriptor.cut + 1,
        ),
        (
            ConnId(106),
            id("root"),
            descriptor.token.clone(),
            descriptor.cut,
        ),
        (
            conn,
            id("not-root"),
            descriptor.token.clone(),
            descriptor.cut,
        ),
    ];
    for (ready_conn, agent_id, token, cut) in wrong {
        w.fx.handle.input(
            ready_conn,
            &marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
                agent_id,
                token,
                cut,
            }),
        );
        assert!(captured.try_iter().next().is_none());
        assert!(host.pane_replay_reserved(conn, &descriptor.token, descriptor.cut));
    }

    w.fx.handle.input(
        conn,
        &marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
            agent_id: id("root"),
            token: descriptor.token.clone(),
            cut: descriptor.cut,
        }),
    );
    let observed = captured
        .try_iter()
        .next()
        .expect("exact Ready starts replay");
    host.unlisten(conn);
    host.shutdown().unwrap();

    let frame = Frame::from_line(std::str::from_utf8(&observed).unwrap()).unwrap();
    assert!(matches!(
        frame,
        Frame::Notification(note)
            if matches!(note.event, Event::NodePaneFrame(ref frame)
                if frame.seq == 0
                    && matches!(frame.frame, marion_core::proto::PaneFrameKindV1::Resize { cols: 80, rows: 24 }))
    ));
}

/// Read `node/pty` notifications until `want` appears in the accumulated bytes.
///
/// Returns `None` on timeout rather than hanging, so a test that proves a *negative* — a
/// read-only client's keystrokes never landing — is a test that can fail rather than one that
/// can only time out.
fn pty_until(
    r: &mut std::io::BufReader<std::os::unix::net::UnixStream>,
    want: &str,
    bound: std::time::Duration,
) -> Option<String> {
    use std::io::BufRead;
    let deadline = std::time::Instant::now() + bound;
    let mut seen = String::new();
    while std::time::Instant::now() < deadline {
        let mut line = String::new();
        // **The socket's own read timeout is not this bound**, and reading through
        // `next_frame` would make it so — its `unwrap` turns a quiet second into a panic. A
        // quiet second is exactly what the negative case (a read-only client's keystrokes
        // never landing) *is*, so it has to be an ordinary outcome here rather than a failure.
        match r.read_line(&mut line) {
            Ok(0) => return None,
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => panic!("reading the attach socket: {e}"),
        }
        match Frame::from_line(&line).expect("well-formed") {
            Frame::Notification(n) => {
                if let Event::NodePty { bytes, .. } = n.event {
                    seen.push_str(&bytes);
                    if seen.contains(want) {
                        return Some(seen);
                    }
                }
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    None
}

fn alive(pid: i32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // SAFETY: signal 0 performs the existence check and delivers nothing.
    unsafe { kill(pid, 0) == 0 }
}

/// **The whole inbound path in one test**: attach, be given the write half, type, and see the
/// *child* answer.
///
/// The echo the line discipline produces would not prove this — a mutation that wrote to the
/// master and never reached the child would still echo. `got:ping` can only be written by the
/// shell that read the keystroke, so this dies if `node/pty-write` stops reaching the pty.
#[test]
fn an_attached_client_is_given_the_write_half_and_its_keystrokes_reach_the_child() {
    let w = Wired::new("handler-pane-write");
    let _host = pane(&w, "root", "while read line; do echo \"got:$line\"; done");

    let mut c = w.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    let (_, outcome) = attach(&mut c, &mut r, "root", 1);
    let got = attached_ok(outcome);

    let p = got
        .pane
        .expect("a node with a registered pty answers with a pane");
    assert!(p.writable, "the first attacher gets the write half");
    assert_eq!(p.held_by, None);
    assert!(!p.ended, "a live pane has not ended");
    assert_eq!(
        (p.cols, p.rows),
        (80, 24),
        "the size is the master's, read back from the kernel"
    );

    write_keys(&mut c, "root", "ping\r");
    assert!(
        pty_until(&mut r, "got:ping", std::time::Duration::from_secs(10)).is_some(),
        "the keystroke never reached the child"
    );
}

/// A v1 input notification has no response envelope. If its durability evidence is refused,
/// keeping the socket open would tell the terminal that the bytes were accepted even though
/// the master was deliberately never written. The exact negotiated connection must therefore
/// close before any terminal `End`; ordinary node lifetime and the next writer remain intact.
#[test]
fn opaque_input_evidence_refusal_closes_only_that_pane_client_before_end() {
    use std::io::BufRead;

    let w = Wired::new("handler-pane-opaque-evidence-refusal");
    let received = w.dir.join("received.txt");
    let script = format!(
        "stty -echo; printf ready; while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; done",
        received.display()
    );
    let host = pane(&w, "root", &script);
    assert!(
        until(|| host.bytes_read() >= b"ready".len() as u64),
        "the child never entered its input loop"
    );
    let pid = host.child_pid().expect("the pane owns a live child");

    let mut first = w.dial();
    let mut first_reader = std::io::BufReader::new(first.try_clone().unwrap());
    call(
        &mut first,
        Call::NodeAttach(marion_core::proto::params::NodeAttachParams {
            agent_id: id("root"),
            pane_stream: Some(marion_core::proto::params::PaneStreamCapabilityV1::new()),
        }),
        1,
    );
    let attached = loop {
        match next_frame(&mut first_reader) {
            Frame::Response(response) => break attached_ok(response.outcome),
            Frame::Notification(note) => assert!(
                !matches!(note.event, Event::NodePaneFrame(_)),
                "a pane frame preceded its attach response"
            ),
            other => panic!("unexpected attach frame: {other:?}"),
        }
    };
    let pane = attached.pane.expect("the live pane is attachable");
    assert!(pane.writable, "the v1 client must own the input lease");
    let descriptor = pane.pane_ready.expect("v1 replay was reserved");
    let ready = Frame::Input(marion_core::proto::ClientNotification::new(
        marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
            agent_id: id("root"),
            token: descriptor.token,
            cut: descriptor.cut,
        }),
    ));
    first.write_all(ready.to_line().as_bytes()).unwrap();
    first.flush().unwrap();
    for expected in 0..descriptor.cut {
        let Frame::Notification(note) = next_frame(&mut first_reader) else {
            panic!("the advertised replay was not delivered")
        };
        let Event::NodePaneFrame(frame) = note.event else {
            panic!("the replay emitted a non-pane notification")
        };
        assert_eq!(frame.seq, expected);
        assert!(
            !matches!(frame.frame, marion_core::proto::PaneFrameKindV1::End {}),
            "a live pane ended during its retained prefix"
        );
    }

    host.fail_next_durable_append("injected opaque input evidence refusal");
    first_reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    write_opaque_keys(&mut first, "root", b"refused\r");
    loop {
        let mut line = String::new();
        match first_reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let frame = Frame::from_line(&line).expect("outbound remains framed");
                assert!(
                    !matches!(
                        frame,
                        Frame::Notification(note)
                            if matches!(note.event, Event::NodePaneFrame(ref pane)
                                if matches!(pane.frame, marion_core::proto::PaneFrameKindV1::End {}))
                    ),
                    "the refusal was forged into a successful terminal End"
                );
            }
            Err(error) => {
                panic!("the opaque notification failed without visibly closing its socket: {error}")
            }
        }
    }
    assert!(
        until(|| w.fx.handle.attachments() == 0 && host.writer().is_none()),
        "gone did not release the failed connection's event cursor and write lease"
    );
    assert!(alive(pid), "input evidence failure killed the node");

    let mut second = w.dial();
    let mut second_reader = std::io::BufReader::new(second.try_clone().unwrap());
    let pane = attached_ok(attach(&mut second, &mut second_reader, "root", 2).1)
        .pane
        .expect("the node remains attachable");
    assert!(
        pane.writable,
        "the next client did not receive the released lease"
    );
    write_keys(&mut second, "root", "accepted\r");
    assert!(
        until(|| std::fs::read_to_string(&received)
            .is_ok_and(|contents| contents.contains("accepted\n"))),
        "the surviving node did not receive the next writer's input"
    );
    assert_eq!(
        std::fs::read_to_string(&received).unwrap(),
        "accepted\n",
        "bytes refused before durable evidence still reached the child"
    );
}

/// A pane `End` is a successful terminal-stream claim. It cannot overtake an opaque input
/// notification admitted from the real v1 socket: if that write later fails, the exact client
/// must depart visibly before an `End` can erase the failure as a clean finish.
#[test]
fn admitted_wire_opaque_input_failure_wins_over_terminal_end() {
    use std::io::BufRead;

    let w = Wired::new("handler-pane-opaque-close-race");
    let host = pane(&w, "root", "sleep 30");
    let mut client = w.dial();
    let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
    call(
        &mut client,
        Call::NodeAttach(marion_core::proto::params::NodeAttachParams {
            agent_id: id("root"),
            pane_stream: Some(marion_core::proto::params::PaneStreamCapabilityV1::new()),
        }),
        1,
    );
    let attached = loop {
        match next_frame(&mut reader) {
            Frame::Response(response) => break attached_ok(response.outcome),
            Frame::Notification(note) => assert!(
                !matches!(note.event, Event::NodePaneFrame(_)),
                "a pane frame preceded its attach response"
            ),
            other => panic!("unexpected attach frame: {other:?}"),
        }
    };
    let pane = attached.pane.expect("a live pane is negotiated");
    assert!(pane.writable);
    let descriptor = pane.pane_ready.expect("v1 replay was reserved");
    let ready = Frame::Input(marion_core::proto::ClientNotification::new(
        marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
            agent_id: id("root"),
            token: descriptor.token,
            cut: descriptor.cut,
        }),
    ));
    client.write_all(ready.to_line().as_bytes()).unwrap();
    client.flush().unwrap();
    for _ in 0..descriptor.cut {
        let Frame::Notification(note) = next_frame(&mut reader) else {
            panic!("the advertised replay was not delivered")
        };
        assert!(matches!(note.event, Event::NodePaneFrame(_)));
    }

    let (admitted_tx, admitted_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    host.set_control_hook(Box::new(move || {
        admitted_tx.send(()).expect("the assertion side is alive");
        release_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(Duration::from_secs(2))
            .expect("the admitted opaque write was not released");
    }));
    host.fail_next_master_input("injected close-race master delivery refusal");
    write_opaque_keys(&mut client, "root", b"late\r");
    admitted_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("wire opaque input was not admitted at the host boundary");

    w.fx.handle.closing_pane(&id("root"), &host);
    let closing = {
        let host = Arc::clone(&host);
        std::thread::spawn(move || host.shutdown())
    };
    reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => panic!("the socket closed before the admitted write reported its failure"),
        Ok(_) => panic!("terminal output overtook the still-admitted opaque write: {line}"),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) => {}
        Err(error) => panic!("reading the negotiated pane socket: {error}"),
    }

    release_tx.send(()).expect("the socket delivery is alive");
    let _ = closing.join().unwrap();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let frame = Frame::from_line(&line).expect("outbound remains framed");
                assert!(
                    !matches!(
                        frame,
                        Frame::Notification(note)
                            if matches!(note.event, Event::NodePaneFrame(ref pane)
                                if matches!(pane.frame, marion_core::proto::PaneFrameKindV1::End {}))
                    ),
                    "a failed admitted input was followed by a successful End"
                );
            }
            Err(error) => {
                panic!("failed admitted input did not visibly close its exact socket: {error}")
            }
        }
    }
}

/// Grace expiry is a terminal outcome for an admitted delivery, not permission to publish a
/// successful pane `End`. This uses a Pending v1 slot and blocks after evidence, when the
/// control permit has already dropped: shutdown must pass `wait_drained`, expire the delivery
/// grace, and close this exact socket before the master outcome is released.
#[test]
fn unresolved_pending_wire_input_is_visibly_failed_before_terminal_end() {
    use std::io::BufRead;

    let w = Wired::new("handler-pane-unresolved-input-grace");
    let host = pane(&w, "root", "sleep 30");
    let mut client = w.dial();
    let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
    call(
        &mut client,
        Call::NodeAttach(marion_core::proto::params::NodeAttachParams {
            agent_id: id("root"),
            pane_stream: Some(marion_core::proto::params::PaneStreamCapabilityV1::new()),
        }),
        1,
    );
    let attached = loop {
        match next_frame(&mut reader) {
            Frame::Response(response) => break attached_ok(response.outcome),
            Frame::Notification(note) => assert!(
                !matches!(note.event, Event::NodePaneFrame(_)),
                "a pane frame preceded its attach response"
            ),
            other => panic!("unexpected attach frame: {other:?}"),
        }
    };
    let pane = attached.pane.expect("a live pane is negotiated");
    assert!(pane.writable);
    assert!(pane.pane_ready.is_some(), "the v1 slot remains Pending");

    let (admitted_tx, admitted_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    host.set_post_cast_input_hook(Box::new(move || {
        admitted_tx
            .send(())
            .expect("the assertion side remains alive");
        release_rx
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .recv_timeout(Duration::from_secs(2))
            .expect("the unresolved master outcome was not released");
    }));
    write_opaque_keys(&mut client, "root", b"pending\r");
    admitted_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("Pending wire input never reached the post-evidence seam");

    w.fx.handle.closing_pane(&id("root"), &host);
    let closing = {
        let host = Arc::clone(&host);
        std::thread::spawn(move || host.shutdown())
    };
    reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let before_release = loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break Ok(()),
            Ok(_) => {
                let frame = Frame::from_line(&line).expect("outbound remains framed");
                if matches!(
                    frame,
                    Frame::Notification(note)
                        if matches!(note.event, Event::NodePaneFrame(ref pane)
                            if matches!(pane.frame, marion_core::proto::PaneFrameKindV1::End {}))
                ) {
                    break Err("terminal End preceded the grace-expiry refusal".to_string());
                }
            }
            Err(error) => {
                break Err(format!(
                    "the unresolved exact socket stayed open past delivery grace: {error}"
                ));
            }
        }
    };

    // Cleanup cannot cause the observation above: the socket outcome was read to completion
    // while the delivery hook still held the unresolved master write.
    release_tx
        .send(())
        .expect("the blocked delivery remains alive for cleanup");
    let _ = closing.join().unwrap();
    before_release.expect("grace expiry must visibly fail the exact socket before End");
}

/// The pane-v1 keyboard's happy path, over the real socket: a negotiated client's very first
/// `node/pane-write`, sent the moment the Ready handshake completes, reaches the child's stdin.
///
/// Every other opaque-input test in this module is a refusal. None of them asserted that an
/// *admitted* write is delivered, so a path that admitted and then lost the bytes — or a
/// client whose first keystroke raced the slot into an inadmissible phase — would have had no
/// test to fail. The oracle is what the child read, not the `i` record: pane-v1 input is
/// evidenced by length only (`PtyHost::write_opaque_input_admitted`), so `pty.cast` carries no
/// `i` for it by design, and that is asserted too so nobody reaches for it as an oracle again.
#[test]
fn a_negotiated_clients_first_opaque_keystroke_reaches_the_child() {
    let w = Wired::new("handler-pane-opaque-first-key");
    let received = w.dir.join("first-key-received.txt");
    let script = format!(
        "stty -echo; printf ready; while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; done",
        received.display()
    );
    let host = pane(&w, "root", &script);
    assert!(
        until(|| host.bytes_read() >= b"ready".len() as u64),
        "the child never entered its input loop"
    );

    let mut client = w.dial();
    let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
    call(
        &mut client,
        Call::NodeAttach(marion_core::proto::params::NodeAttachParams {
            agent_id: id("root"),
            pane_stream: Some(marion_core::proto::params::PaneStreamCapabilityV1::new()),
        }),
        1,
    );
    let attached = loop {
        match next_frame(&mut reader) {
            Frame::Response(response) => break attached_ok(response.outcome),
            Frame::Notification(note) => assert!(
                !matches!(note.event, Event::NodePaneFrame(_)),
                "a pane frame preceded its attach response"
            ),
            other => panic!("unexpected attach frame: {other:?}"),
        }
    };
    let pane = attached.pane.expect("the live pane is attachable");
    assert!(pane.writable, "the v1 client must own the input lease");
    let descriptor = pane.pane_ready.expect("v1 replay was reserved");
    let ready = Frame::Input(marion_core::proto::ClientNotification::new(
        marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
            agent_id: id("root"),
            token: descriptor.token,
            cut: descriptor.cut,
        }),
    ));
    client.write_all(ready.to_line().as_bytes()).unwrap();
    client.flush().unwrap();
    // Typed immediately behind Ready, the way `marion attach` starts its keyboard: no replay
    // frame is waited for first, so this is the earliest a real client can type.
    write_opaque_keys(&mut client, "root", b"typed\r");

    assert!(
        until(|| std::fs::read_to_string(&received)
            .is_ok_and(|contents| contents.contains("typed\n"))),
        "the first admitted opaque keystroke never reached the child. Received: {:?}",
        std::fs::read_to_string(&received).unwrap_or_default()
    );
    let cast = std::fs::read_to_string(w.dir.join("root.cast")).unwrap();
    assert!(
        !cast.contains("\"i\""),
        "pane-v1 input is evidenced by length only; an `i` record means the opaque path \
         started persisting raw keyboard payloads:\n{cast}"
    );
    // The socket is still a live, framed connection: nothing about the delivery departed it.
    call(
        &mut client,
        Call::NodeGet(marion_core::proto::params::NodeGetParams::of(id("root"))),
        2,
    );
    loop {
        match next_frame(&mut reader) {
            Frame::Response(_) => break,
            Frame::Notification(note) => assert!(
                matches!(note.event, Event::NodePaneFrame(_)),
                "an unexpected notification followed the keystroke: {note:?}"
            ),
            other => panic!("the admitted keystroke closed the connection: {other:?}"),
        }
    }
}

/// `node/pane-write` is negotiated protocol, not an alternate spelling for legacy input. A
/// legacy writer has a keyboard lease but no pane stream slot; forged opaque bytes must reach
/// neither the master nor silence, so the exact legacy socket is visibly closed.
#[test]
fn legacy_attach_cannot_forge_opaque_pane_input() {
    use std::io::BufRead;

    let w = Wired::new("handler-pane-forged-opaque");
    let received = w.dir.join("forged-received.txt");
    let script = format!(
        "stty -echo; printf ready; while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; done",
        received.display()
    );
    let host = pane(&w, "root", &script);
    assert!(until(|| host.bytes_read() >= b"ready".len() as u64));
    let mut client = w.dial();
    let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
    assert!(
        attached_ok(attach(&mut client, &mut reader, "root", 1).1)
            .pane
            .expect("the pane is live")
            .writable
    );

    write_opaque_keys(&mut client, "root", b"forged\r");
    reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut line = String::new();
    assert_eq!(
        reader
            .read_line(&mut line)
            .expect("the socket read is bounded"),
        0,
        "unnegotiated opaque input was ignored instead of visibly refused"
    );
    assert!(
        until(|| host.writer().is_none()),
        "gone did not release the forged sender's lease"
    );
    assert!(
        !received.exists() || std::fs::read(&received).unwrap().is_empty(),
        "unnegotiated opaque input reached the child"
    );
}

#[test]
fn legacy_input_keeps_its_compatibility_delivery_on_durable_evidence_failure() {
    let w = Wired::new("handler-pane-legacy-evidence-failure");
    let host = pane(
        &w,
        "root",
        "stty -echo; while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done",
    );
    let mut client = w.dial();
    let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
    let attached = attached_ok(attach(&mut client, &mut reader, "root", 1).1);
    assert!(attached.pane.expect("the pane is live").writable);

    host.fail_next_durable_append("injected legacy evidence failure");
    write_keys(&mut client, "root", "accepted\r");
    assert!(
        pty_until(&mut reader, "got:accepted", Duration::from_secs(10)).is_some(),
        "the legacy compatibility path stopped delivering after evidence failure"
    );
    call(
        &mut client,
        Call::NodeGet(marion_core::proto::params::NodeGetParams::of(id("root"))),
        2,
    );
    // The pane's output can still be arriving after the line the test waited for; the
    // connection is open if the call is answered after it.
    let answered = loop {
        match next_frame(&mut reader) {
            Frame::Notification(_) => continue,
            other => break matches!(other, Frame::Response(_)),
        }
    };
    assert!(answered, "legacy evidence failure closed the connection");
}

/// §5.3's one-writer rule, over the socket and **by name**.
///
/// Two halves, and the second is the one that matters: the refusal is not merely reported, it
/// is enforced. A supervisor that answered `writable: false` and then wrote the bytes anyway
/// would pass a test that only read the response.
#[test]
fn a_second_attacher_is_refused_the_write_half_by_name_and_cannot_type() {
    let w = Wired::new("handler-pane-second");
    let _host = pane(&w, "root", "while read line; do echo \"got:$line\"; done");

    let mut first = w.dial();
    let mut fr = std::io::BufReader::new(first.try_clone().unwrap());
    let (_, outcome) = attach(&mut first, &mut fr, "root", 1);
    let a = attached_ok(outcome).pane.expect("a pane");
    assert!(a.writable);

    let mut second = w.dial();
    let mut sr = std::io::BufReader::new(second.try_clone().unwrap());
    let (_, outcome) = attach(&mut second, &mut sr, "root", 1);
    let b = attached_ok(outcome).pane.expect("a pane");
    assert!(
        !b.writable,
        "two writers on one pty interleave into nonsense"
    );
    assert!(
        b.held_by.is_some(),
        "a client told only `busy` cannot tell a colleague from a lease nobody released"
    );

    // The refusal is enforced, not merely announced.
    write_keys(&mut second, "root", "sneak\r");
    assert!(
        pty_until(&mut sr, "got:sneak", std::time::Duration::from_secs(2)).is_none(),
        "a read-only client typed into the node anyway"
    );
    // And the writer still works, so the block is about the lease and not about the socket.
    write_keys(&mut first, "root", "ping\r");
    assert!(
        pty_until(&mut fr, "got:ping", std::time::Duration::from_secs(10)).is_some(),
        "refusing the second client broke the first"
    );
}

/// **A resize reaches the pty and the child sees it as a `SIGWINCH`.**
///
/// The `stty size` the trap prints is read by the shell *through its controlling terminal*, so
/// it is the kernel's answer and not marion's: this dies if `node/resize` stops reaching
/// `TIOCSWINSZ`, and it dies if the explicit `killpg(SIGWINCH)` stops being sent to a child
/// that would otherwise never look.
#[test]
fn a_resize_reaches_the_pty_and_the_child_is_told() {
    let w = Wired::new("handler-pane-resize");
    let host = pane(
        &w,
        "root",
        // **`ready` is printed after the trap is installed, and only once the test has typed
        // `go` through its attach**, and the test waits for it. A signal delivered to a shell
        // that has not reached its `trap` yet is simply lost, so without `ready` the test races
        // the child's startup. And without `go`, a quick child prints `ready` before the attach
        // lands: legacy attach replays the node's event stream, never earlier pty bytes, so
        // that `ready` never reaches this client and the wait for it expires. That flaked
        // on macOS CI.
        "stty -echo; trap 'stty size' WINCH; read go; echo ready; while :; do sleep 0.05; done",
    );

    let mut c = w.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    let (_, outcome) = attach(&mut c, &mut r, "root", 1);
    assert!(attached_ok(outcome).pane.expect("a pane").writable);
    write_keys(&mut c, "root", "go\r");
    assert!(
        pty_until(&mut r, "ready", std::time::Duration::from_secs(10)).is_some(),
        "the child never installed its WINCH trap"
    );

    send_resize(&mut c, "root", 140, 40);
    // `stty size` prints "rows cols".
    assert!(
        pty_until(&mut r, "40 140", std::time::Duration::from_secs(10)).is_some(),
        "the child was never told the new size"
    );
    let size = host.master().size().expect("TIOCGWINSZ");
    assert_eq!((size.cols, size.rows), (140, 40), "the master itself moved");
}

/// A read-only attacher must not resize either. The geometry is the *shared* master's, so a
/// second client reflowing it would repaint the writer's pane from under them with nothing
/// anywhere naming the cause — the same invisibility §5.3 gives for interleaved keystrokes.
#[test]
fn a_read_only_attacher_cannot_resize_the_node() {
    let w = Wired::new("handler-pane-resize-ro");
    let host = pane(&w, "root", "sleep 30");

    let mut first = w.dial();
    let mut fr = std::io::BufReader::new(first.try_clone().unwrap());
    attached_ok(attach(&mut first, &mut fr, "root", 1).1);

    let mut second = w.dial();
    let mut sr = std::io::BufReader::new(second.try_clone().unwrap());
    assert!(
        !attached_ok(attach(&mut second, &mut sr, "root", 1).1)
            .pane
            .expect("a pane")
            .writable
    );

    send_resize(&mut second, "root", 200, 60);
    // Nothing to wait *for*, so wait for the supervisor to have processed something later on
    // the same connection instead of sleeping on a hope.
    call(
        &mut second,
        Call::NodeGet(marion_core::proto::params::NodeGetParams::of(id("root"))),
        9,
    );
    assert!(matches!(next_frame(&mut sr), Frame::Response(_)));
    let size = host.master().size().expect("TIOCGWINSZ");
    assert_eq!(
        (size.cols, size.rows),
        (80, 24),
        "a read-only client reflowed the writer's pane"
    );
}

/// **Detach leaves the node running and hands the keyboard back.**
///
/// This is M2's property at the surface M3 adds, and both halves are asserted because they
/// fail independently: a supervisor that killed the node on departure would pass the lease
/// half, and one that never released the lease would leave the node permanently read-only
/// while the process ran on.
#[test]
fn a_client_departing_leaves_the_node_running_and_releases_its_keyboard() {
    let w = Wired::new("handler-pane-detach");
    let host = pane(&w, "root", "while read line; do echo \"got:$line\"; done");
    let pid = host.child_pid().expect("a child");

    let mut first = w.dial();
    let mut fr = std::io::BufReader::new(first.try_clone().unwrap());
    assert!(
        attached_ok(attach(&mut first, &mut fr, "root", 1).1)
            .pane
            .expect("a pane")
            .writable
    );

    // The client goes away without saying anything — §7.3.1's crash case, which is also what
    // `^] d` looks like from here once the attach process exits.
    drop(fr);
    drop(first);
    assert!(
        until(|| w.fx.handle.attachments() == 0),
        "the supervisor never noticed the departure"
    );

    assert!(alive(pid), "the node was killed by a client going away");
    let mut second = w.dial();
    let mut sr = std::io::BufReader::new(second.try_clone().unwrap());
    let p = attached_ok(attach(&mut second, &mut sr, "root", 1).1)
        .pane
        .expect("a pane");
    assert!(
        p.writable,
        "the departed client's lease was never released: {:?}",
        p.held_by
    );
    write_keys(&mut second, "root", "ping\r");
    assert!(
        pty_until(&mut sr, "got:ping", std::time::Duration::from_secs(10)).is_some(),
        "the node survived but nobody can type into it"
    );
    assert!(alive(pid), "the node is still the same process");
}

/// A node with no display plane answers `pane: None`, and that is a fact about the node rather
/// than a failure of the attach — every other node in this file is one, and none of them
/// regressed.
#[test]
fn a_node_with_no_pty_attaches_with_no_pane() {
    let w = Wired::new("handler-pane-none");
    let stream = events_of(&w.fx, "root");
    say(&stream, "root", &["one"]);
    let mut c = w.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    let got = attached_ok(attach(&mut c, &mut r, "root", 1).1);
    assert_eq!(got.pane, None);
    assert_eq!(w.fx.handle.panes(), 0);
    // And a keystroke aimed at it is dropped rather than answered, crashing nothing.
    write_keys(&mut c, "root", "x");
    call(
        &mut c,
        Call::NodeGet(marion_core::proto::params::NodeGetParams::of(id("root"))),
        2,
    );
    assert!(matches!(next_frame(&mut r), Frame::Response(_)));
}

/// **The whole of §7.3.3, on one connection**: everything written while nobody was listening
/// arrives as replay, the answer's read point is a statement about what has *already* been
/// sent, and what the node says next arrives unsolicited on the same socket.
///
/// The contiguity assertion is the seam. `events.rs` argues it is unreachable to get wrong
/// because replay and subscribe are one cursor; this is that argument being spent — the
/// ordinals across the two legs are `0..5` with nothing missing and nothing twice.
#[test]
fn node_attach_replays_the_detached_window_and_then_follows_the_same_cursor_live() {
    let w = Wired::new("handler-attach");
    let stream = events_of(&w.fx, "root");
    say(&stream, "root", &["before-1", "before-2", "before-3"]);

    let mut c = w.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    let (replay, outcome) = attach(&mut c, &mut r, "root", 1);
    let got = attached_ok(outcome);

    assert_eq!(got.node.agent_id, id("root"));
    assert!(
        got.mode.is_live(),
        "a node that has not exited is a re-subscribe: {:?}",
        got.mode
    );
    assert_eq!(
        heard(&replay),
        vec![
            (0, "before-1".into()),
            (1, "before-2".into()),
            (2, "before-3".into())
        ],
        "the detached window is replayed in full, in order, exactly once"
    );
    assert_eq!(
        got.mode.replay_point().records,
        3,
        "the read point counts what the client has already been sent, not what it may expect"
    );

    // Written by a *second* writer, after the attach — which is the production case: the
    // supervisor is not the process driving this node.
    say(&stream, "root", &["after-1", "after-2", "after-3"]);
    let mut live = Vec::new();
    while live.len() < 3 {
        let Frame::Notification(n) = next_frame(&mut r) else {
            panic!("live events arrive as notifications on the connection the client has")
        };
        live.push(n.event);
    }
    assert_eq!(
        heard(&live),
        vec![
            (3, "after-1".into()),
            (4, "after-2".into()),
            (5, "after-3".into())
        ],
        "the subscribe leg continues the replay's own ordinals: no gap, no repeat, no join"
    );
}

/// **An attachment is woken by its file, not read on a timer.** While a client follows a node
/// the handler asks for no deadline at all, and a second writer's append to the node's
/// `events.jsonl` is what wakes the accept loop — through the same `changes` signal its wake
/// pipe is attached to.
#[test]
fn an_attached_node_asks_for_no_tick_and_its_file_wakes_the_loop() {
    let w = Wired::new("handler-attach-watch");
    let stream = events_of(&w.fx, "root");
    say(&stream, "root", &["before"]);
    let mut c = w.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    let (_replay, outcome) = attach(&mut c, &mut r, "root", 1);
    attached_ok(outcome);
    assert_eq!(
        w.fx.handle.next_deadline(),
        None,
        "a followed node is not a reason to tick"
    );
    // Past the watch thread's own arming notify, so only the append can move the generation.
    std::thread::sleep(std::time::Duration::from_millis(100));
    let changes = w.fx.handle.live.changes();
    let seen = changes.generation();
    let waiter =
        std::thread::spawn(move || changes.wait_past(seen, std::time::Duration::from_secs(5)));
    std::thread::sleep(std::time::Duration::from_millis(50));
    say(&stream, "root", &["after"]);
    assert_ne!(
        waiter.join().unwrap(),
        seen,
        "the append woke the loop's signal before the bound"
    );
}

/// **Two clients, two cursors.** A shared reader would make the second client's replay depend
/// on when the first attached, which is the same collapse `events.rs` refuses between "nobody
/// read this" and "there was nothing to read".
#[test]
fn a_second_client_attaching_later_gets_its_own_replay_from_the_beginning() {
    let w = Wired::new("handler-attach-two");
    let stream = events_of(&w.fx, "root");
    say(&stream, "root", &["one", "two"]);

    let mut a = w.dial();
    let mut ar = std::io::BufReader::new(a.try_clone().unwrap());
    let (first, outcome) = attach(&mut a, &mut ar, "root", 1);
    attached_ok(outcome);
    assert_eq!(heard(&first).len(), 2);

    say(&stream, "root", &["three"]);
    // A's live leg, drained so the two clients cannot be confused for one.
    let Frame::Notification(_) = next_frame(&mut ar) else {
        panic!("A hears the third event")
    };

    let mut b = w.dial();
    let mut br = std::io::BufReader::new(b.try_clone().unwrap());
    let (second, outcome) = attach(&mut b, &mut br, "root", 1);
    let got = attached_ok(outcome);
    assert_eq!(
        heard(&second),
        vec![(0, "one".into()), (1, "two".into()), (2, "three".into())],
        "B replays the whole file, not the tail A had not read"
    );
    assert_eq!(got.mode.replay_point().records, 3);
    assert!(until(|| w.fx.handle.attachments() == 2));
}

/// **A node marion never recorded is not a node that said nothing** — and the two are only
/// distinguishable while something can still be written, which is why the answer turns on
/// whether the node has exited.
#[test]
fn an_exited_node_with_no_stream_is_refused_as_unrecorded_rather_than_replayed_as_silent() {
    let w = Wired::new("handler-attach-unrecorded");
    append(
        &w.fx.path,
        &line(
            9,
            9_000,
            RecordKind::Exited(Exited {
                agent_id: id("root"),
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "finished while nobody was attached".into(),
                },
            }),
        ),
    );
    assert!(until(|| {
        w.fx.handle.live.refresh();
        w.fx.handle
            .live
            .read(|r| r.tree().get(&id("root")).unwrap().state.is_exited())
    }));

    let mut c = w.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    let (notes, outcome) = attach(&mut c, &mut r, "root", 1);
    assert!(notes.is_empty(), "nothing was replayed");
    let e = refusal(outcome);
    assert_eq!(e.kind(), Some(FailureKind::NotFound));
    assert!(
        e.message.contains("not** an empty transcript"),
        "the refusal names which of the two facts it is: {e}"
    );
    assert_eq!(
        w.fx.handle.attachments(),
        0,
        "a refused attach leaves no cursor behind"
    );
}

/// The other side of the same split: a node that has **not** exited and has written nothing yet
/// is followed, because its file appears on its first frame and the reader is already watching
/// the name. Refusing here would make a client unable to attach to a node that is starting —
/// the one an operator is most likely watching (§2's own argument for `tree/node-added`).
#[test]
fn a_live_node_that_has_not_spoken_yet_is_followed_and_its_first_frame_arrives() {
    let w = Wired::new("handler-attach-silent");
    let mut c = w.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    let (notes, outcome) = attach(&mut c, &mut r, "root", 1);
    let got = attached_ok(outcome);
    assert!(notes.is_empty());
    assert!(got.mode.is_live());
    assert_eq!(got.mode.replay_point().records, 0);

    say(&events_of(&w.fx, "root"), "root", &["first-word"]);
    let Frame::Notification(n) = next_frame(&mut r) else {
        panic!("the first frame of a node that had not spoken still reaches its client")
    };
    assert_eq!(heard(&[n.event]), vec![(0, "first-word".into())]);
}

/// A node the journal has no record of. The refusal carries the read point for the same reason
/// `node/get`'s does: *"no such node"* and *"not yet"* are different answers.
#[test]
fn attaching_to_a_node_the_journal_never_recorded_is_a_refusal_that_says_how_much_was_read() {
    let w = Wired::new("handler-attach-missing");
    let mut c = w.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    let (notes, outcome) = attach(&mut c, &mut r, "nobody", 1);
    assert!(notes.is_empty());
    let e = refusal(outcome);
    assert_eq!(e.kind(), Some(FailureKind::NotFound));
    assert!(e.is_refusal());
    assert!(e.message.contains("records read"), "{e}");
    assert_eq!(w.fx.handle.attachments(), 0);
}

/// **§7.2 meets §7.3.3.** An `Orphaned` node is one whose channel *no* supervisor holds: this
/// one booted over a journal it did not write, and `restart.rs` marked the node because the
/// record shows no decision about its fate. `ResubscribeFrom` asserts the opposite — that the
/// supervisor has held the channel since `t=0` — so answering it here would promise live events
/// that can never arrive. The orphan is a replayable record whose operator option is to bring it
/// back, which is exactly what `ReplayResumable` says; that holds whether the orphan's last
/// state was mid-turn or, per §7.2's *"the process may be gone"*, an exit the record never saw.
#[test]
fn an_orphaned_node_attaches_as_replay_resumable_and_never_as_a_live_channel() {
    let p = || ReplayPoint {
        records: 7,
        src_seq: None,
    };
    for state in [
        NodeState::Running,
        NodeState::Idle,
        NodeState::Exited(ExitStatus::Ok),
    ] {
        let mode = attach_mode(state, ReapState::Orphaned, p());
        assert!(
            matches!(mode, AttachMode::ReplayResumable(_)),
            "an orphan in state {state:?} answered {mode:?}"
        );
        assert!(!mode.is_live(), "no supervisor holds an orphan's channel");
        assert_eq!(mode.replay_point(), &p());
    }
}

/// §7.3.3's three answers, at the one place that chooses between them.
#[test]
fn the_attach_mode_is_derived_from_the_nodes_two_state_fields_and_nothing_else() {
    let p = || ReplayPoint {
        records: 4,
        src_seq: None,
    };
    assert!(matches!(
        attach_mode(NodeState::Running, ReapState::Live, p()),
        AttachMode::ResubscribeFrom(_)
    ));
    assert!(matches!(
        attach_mode(NodeState::Exited(ExitStatus::Ok), ReapState::Live, p()),
        AttachMode::ReplayOnly(_)
    ));
    // Reaped wins over exited: §7.3.2's disposition (c) reaps an *idle* node, and the operator's
    // option — bring it back — is what `ReplayResumable` exists to say.
    assert!(matches!(
        attach_mode(NodeState::Idle, ReapState::ReapedIdle, p()),
        AttachMode::ReplayResumable(_)
    ));
    assert!(matches!(
        attach_mode(
            NodeState::Exited(ExitStatus::Ok),
            ReapState::ReapedIdle,
            p()
        ),
        AttachMode::ReplayResumable(_)
    ));
    assert_eq!(
        attach_mode(NodeState::Running, ReapState::Live, p()).replay_point(),
        &p(),
        "the point is carried, never recomputed"
    );
}

/// **§7.3.1, on the attach path.** A client that leaves takes its cursor with it and nothing
/// else: the node goes on writing, which is what makes the next client's attach a replay.
#[test]
fn a_departed_client_stops_being_followed_and_the_node_keeps_recording() {
    let w = Wired::new("handler-attach-gone");
    let stream = events_of(&w.fx, "root");
    say(&stream, "root", &["one"]);
    {
        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        let (replay, outcome) = attach(&mut c, &mut r, "root", 1);
        attached_ok(outcome);
        assert_eq!(heard(&replay).len(), 1);
        assert!(until(|| w.fx.handle.attachments() == 1));
    }
    assert!(
        until(|| w.fx.handle.attachments() == 0),
        "the cursor is dropped when its connection ends"
    );

    say(&stream, "root", &["two", "three"]);
    let mut c = w.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    let (replay, outcome) = attach(&mut c, &mut r, "root", 1);
    attached_ok(outcome);
    assert_eq!(
        heard(&replay),
        vec![(0, "one".into()), (1, "two".into()), (2, "three".into())],
        "everything written across the detached window is there, exactly once"
    );
}

/// An event marion **read and deliberately did not keep** is still delivered as the fact it is.
/// The alternative — dropping it — would make a client's `agent_seq` run non-contiguous, which
/// is the one signal it has for loss.
#[test]
fn a_withheld_payload_is_delivered_as_a_withholding_not_omitted_from_the_stream() {
    let w = Wired::new("handler-attach-withheld");
    let stream = events_of(&w.fx, "root");
    {
        let mut writer = EventWriter::open_path(&stream, &id("root")).unwrap();
        writer.record(Draft::observed(
            Payload::Raw("one".into()),
            Source::Protocol,
        ));
        writer.record(Draft::observed(
            Payload::Withheld {
                key: "control_response".into(),
                bytes: 30_000,
                reason: "§5.2".into(),
            },
            Source::Protocol,
        ));
        writer.record(Draft::observed(
            Payload::Oversized {
                was: PayloadKind::Vendor,
                bytes: 1,
            },
            Source::Protocol,
        ));
        writer.sync().unwrap();
    }
    let mut c = w.dial();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    let (replay, outcome) = attach(&mut c, &mut r, "root", 1);
    attached_ok(outcome);
    let seqs: Vec<u64> = replay
        .iter()
        .map(|e| match e {
            Event::NodeEvent { agent_seq, .. } => *agent_seq,
            other => panic!("{}", other.method()),
        })
        .collect();
    assert_eq!(seqs, vec![0, 1, 2], "no ordinal is skipped");
    let Event::NodeEvent { payload, .. } = &replay[1] else {
        unreachable!()
    };
    assert!(payload.get("Withheld").is_some(), "{payload}");
    let Event::NodeEvent { payload, .. } = &replay[2] else {
        unreachable!()
    };
    assert!(payload.get("Oversized").is_some(), "{payload}");
}

/// **§11 item 28 step 4** — the supervisor owning a node's lifecycle: the table, the capability
/// token, and `agent/spawn` answered rather than refused.
///
/// Driven by a **test client** and no bridge, which is what makes this landable ahead of step 5.
/// The design's point 4 says (a) and (b) are atomic on the wire — a handler with no client is dead
/// code and a client with no handler cannot spawn — and names exactly this as what can split off.
/// So the client here is `RegistryHandle::call` itself, reached the way `serve` reaches it.
#[cfg(test)]
mod owns_nodes {
    use super::*;
    use marion_core::paths::ProjectDir;
    use marion_core::proto::params::AgentSpawnParams;
    use marion_core::proto::{
        NativeEnvVarV1, NativeLaunchContextV1, NativeLaunchContextV2, OpaqueOsValueV1, SpawnCaller,
        TerminalGeometryV1,
    };
    use marion_testsupport::{fixture_repo, scratch};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    /// A handle that **owns** what it spawns, over a real repo and a real project directory.
    struct Owning {
        handle: Arc<RegistryHandle>,
        project: ProjectDir,
        /// The tree these nodes live in. Held because it is now a per-node fact the fixture
        /// has to state when it claims one — see [`NodeHandle::repo`].
        repo: PathBuf,
        /// **Declared last so it is dropped last.** Rust drops fields in declaration order,
        /// and a scratch directory removed while a node's thread is still writing into it
        /// would turn a clean failure into an unrelated io error.
        _dir: marion_testsupport::Scratch,
    }

    /// **Detach guidance names the socket this supervisor serves, not the journal's
    /// neighbour.** A state root long enough to overflow `sun_path` moves the socket to §2's
    /// `/tmp` fallback while the journal stays under `<state>`, and an operator told to
    /// reconnect to `<state>/…/supervisor.sock` would dial nothing.
    ///
    /// Mutation: derive the path from the journal directory and both lines name the primary.
    #[test]
    fn detach_guidance_names_the_serving_socket_when_it_overflowed_to_tmp() {
        let dir = scratch("handler-overflow-guidance");
        let root = dir.join("repo");
        let state = dir.join("a".repeat(100));
        let paths = crate::socket::socket_paths(&state, &root, crate::socket::own_uid());
        assert!(
            paths.overflow().is_some(),
            "the fixture must overflow: {}",
            state.display()
        );
        let project = ProjectDir::new(&state, &root);
        let primary = project.supervisor_sock();
        assert_ne!(paths.socket(), primary);
        std::fs::create_dir_all(project.path()).unwrap();
        let live = Arc::new(crate::registry::LiveRegistry::follow(
            Registry::boot(&project).unwrap(),
        ));
        let handle = RegistryHandle::owning(
            live,
            crate::run::Env {
                os_sandbox: true,
                project_dir: project.clone(),
                state: state.clone(),
                project_root: root.clone(),
                bridge: std::path::PathBuf::from("/bin/marion-supervisor"),
                base_url: Some("http://127.0.0.1:8099/v1".into()),
                auth: marion_harness::Auth::Canned,
            },
            paths.socket().to_path_buf(),
        );
        let guidance = handle.guidance();
        let serving = format!(" on {}", paths.socket().display());
        for line in [guidance.reattach, guidance.stop_fleet] {
            assert!(line.contains(&serving), "{line}");
            assert!(!line.contains(&primary.display().to_string()), "{line}");
        }
    }

    fn owning(tag: &str, records: Vec<RecordKind>) -> Owning {
        let dir = scratch(tag);
        let repo = fixture_repo(&dir);
        let state = dir.join("state");
        // **Keyed the way production keys it** — `marion.rs` builds every one of the socket, the
        // `Launch` and the `ProjectDir` from a single `socket::project_root(&repo)`, which for a
        // repository is its git common dir and not its working tree. Keying on `repo` here gave
        // the fixture a supervisor whose project no client could name, which nothing noticed
        // until `spawn_root` began comparing the two (the sibling worktree fixture below always
        // keyed correctly, which is why it alone kept passing).
        let project = ProjectDir::new(&state, &crate::socket::project_root(&repo));
        std::fs::create_dir_all(project.path()).unwrap();
        let journal = project.journal();
        // Booted before the records are written, which is the production order — `tests::fx_with`
        // gives the argument, and it matters more here: a node already `Live` at boot is one this
        // supervisor never decided the fate of and `restart.rs` marks `Orphaned`.
        let live = Arc::new(crate::registry::LiveRegistry::follow(
            Registry::boot_path(&journal).unwrap(),
        ));
        for (seq, kind) in records.into_iter().enumerate() {
            append(&journal, &line(seq as u64, 1_000 + seq as u64, kind));
        }
        live.refresh();
        let handle = RegistryHandle::owning(
            live,
            crate::run::Env {
                os_sandbox: true,
                project_dir: project.clone(),
                state: state.clone(),
                project_root: crate::socket::project_root(&repo),
                bridge: std::path::PathBuf::from("/bin/marion-supervisor"),
                // Answers nothing, which is what bounds the two tests below that really launch.
                base_url: Some("http://127.0.0.1:8099/v1".into()),
                auth: marion_harness::Auth::Canned,
            },
            project.supervisor_sock(),
        );
        Owning {
            handle,
            project,
            repo,
            _dir: dir,
        }
    }

    fn params(caller: Option<SpawnCaller>, secs: u64) -> AgentSpawnParams {
        AgentSpawnParams {
            wider_children: None,
            budget_tokens: None,
            review_of: None,
            candidates: vec![],
            race: None,
            notify_parent: false,
            agent_type: "claude".into(),
            prompt: "do the task".into(),
            native_launch: None,
            caller,
            // Root-only, and every caller in this helper's `Some` half would be refused by
            // name for stating it — see `a_caller_that_states_no_change_record_is_refused`.
            no_change_record: None,
            pane: None,
            isolation: None,
            allow_concurrent_writes: None,
            // Every caller in this module is a `Some`, and a `Some` that states a repository
            // is refused by name — see `a_caller_that_states_its_own_repository_is_refused`.
            repo: None,
            acceptance_criteria: vec![],
            verification: vec![],
            writable_scope: vec!["src/**".into()],
            timeout_secs: Some(secs),
            model: None,
            profile: None,
        }
    }

    fn spawn(
        fx: &Owning,
        p: AgentSpawnParams,
    ) -> Result<marion_core::proto::result::AgentSpawnResult, RpcError> {
        let out = crate::serve::sink(ConnId(3));
        fx.handle.hello_as_operator(ConnId(3));
        match fx.handle.call(ConnId(3), &Call::AgentSpawn(p), &out)? {
            MethodResult::AgentSpawn(r) => Ok(r),
            other => panic!("wrong result: {}", other.method().as_str()),
        }
    }

    /// **An owning handle acts on a budget line the moment a published figure crosses it**:
    /// the spend a node's sink publishes reaches the book, and the enforcer thread journals the
    /// crossing — woken by the figure, never by a poll.
    ///
    /// Mutation: build the handle's `Spending` without `enforcing` and nothing is journaled.
    #[test]
    fn a_published_spend_past_a_warn_line_is_journaled_by_the_enforcer() {
        let fx = owning("owns-budget-warn", vec![]);
        let n = id("n-budget");
        fx.handle.budgets.register(
            &n,
            None,
            Some(marion_core::budget::Budget {
                tokens: Some(100),
                ..Default::default()
            }),
            0,
        );
        fx.handle.spending.publish(
            &n,
            crate::spending::Spent {
                usage: Some(marion_core::contract::TokenUsage {
                    input: 85,
                    ..Default::default()
                }),
                turns: vec![],
            },
        );
        let crossed = || {
            std::fs::read_to_string(fx.project.journal())
                .unwrap_or_default()
                .contains("BudgetCrossed")
        };
        assert!(
            marion_testsupport::until_within(
                std::time::Duration::from_secs(10),
                std::time::Duration::from_millis(10),
                crossed
            ),
            "the warn line was never journaled"
        );
    }

    /// A handle over `project` whose notices go to `backend`, and the journal it follows.
    fn notified(
        dir: &std::path::Path,
        backend: crate::notify::Backend,
    ) -> (Arc<RegistryHandle>, PathBuf) {
        // A restart reuses the repository the first handle made.
        let repo = match dir.join("repo") {
            r if r.exists() => r,
            _ => fixture_repo(dir),
        };
        let state = dir.join("state");
        let project = ProjectDir::new(&state, &crate::socket::project_root(&repo));
        std::fs::create_dir_all(project.path()).unwrap();
        let live = Arc::new(crate::registry::LiveRegistry::follow(
            Registry::boot_path(&project.journal()).unwrap(),
        ));
        let notifier = crate::notify::NotifySeed {
            config: crate::notify::NotifyConfig {
                enabled: true,
                finished: crate::notify::Finished::All,
                terminal: crate::notify::TerminalRing::Bell,
            },
            backend,
            project: "repo".into(),
        };
        let handle = RegistryHandle::owning_notified(
            live,
            crate::run::Env {
                os_sandbox: true,
                project_dir: project.clone(),
                state,
                project_root: crate::socket::project_root(&repo),
                bridge: PathBuf::from("/bin/marion-supervisor"),
                base_url: None,
                auth: marion_harness::Auth::Canned,
            },
            project.supervisor_sock(),
            notifier,
        );
        (handle, project.journal())
    }

    fn state_record(agent: &str, state: NodeState) -> RecordKind {
        RecordKind::StateChanged(marion_core::journal::StateChanged {
            agent_id: id(agent),
            state,
            reason: None,
        })
    }

    /// **A node that blocks and then fails is two notices, and a restart replays neither** —
    /// through the real flush, on the `Record` backend, with no client connected.
    #[test]
    fn a_node_that_blocks_then_fails_is_two_notices_and_a_restart_replays_neither() {
        let dir = scratch("owns-notify-record");
        let record = dir.join("notices.jsonl");
        let backend = crate::notify::Backend::Record(record.clone());
        let (handle, journal) = notified(&dir, backend.clone());
        let lines = || {
            std::fs::read_to_string(&record)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        let step = |seq: u64, kind: RecordKind| {
            append(&journal, &line(seq, 1_000 + seq, kind));
            handle.live.refresh();
            handle.flush();
        };
        step(0, intent("n-4242", None, "codex-impl", 0));
        step(
            1,
            state_record(
                "n-4242",
                NodeState::Blocked(marion_core::node::BlockReason::Permission),
            ),
        );
        step(
            2,
            RecordKind::Exited(marion_core::journal::Exited {
                agent_id: id("n-4242"),
                status: marion_core::contract::ExitStatus::Failed,
                exit: marion_core::contract::ProcessExit {
                    code: Some(1),
                    signal: None,
                    description: "boom".into(),
                },
            }),
        );
        assert!(
            marion_testsupport::until_within(
                std::time::Duration::from_secs(10),
                std::time::Duration::from_millis(10),
                || lines().len() >= 2
            ),
            "{:?}",
            lines()
        );
        let told = lines();
        assert_eq!(told.len(), 2, "{told:?}");
        assert!(
            told[0].contains("needs you") && told[1].contains("failed"),
            "{told:?}"
        );
        drop(handle);

        let (restarted, _) = notified(&dir, backend);
        restarted.live.refresh();
        restarted.flush();
        assert_eq!(
            lines().len(),
            2,
            "a restart only learns what is already true"
        );
    }

    /// **A child's typed budget is the one its tree started with**: the parent's types
    /// snapshot, not the repository's live `.marion/agents.toml`, which a node can edit to
    /// lift its children's caps.
    #[test]
    fn a_childs_typed_budget_is_read_from_the_trees_snapshot_not_the_live_file() {
        let fx = owning(
            "owns-budget-snapshot",
            vec![intent("root", None, "claude", 0)],
        );
        let row = |tokens: u64| {
            format!(
                "[[agent]]\nname = \"capped\"\nharness = \"codex\"\ndescription = \"x\"\n\
                 budget = {{ tokens = {tokens} }}\n"
            )
        };
        let file = fx.repo.join(crate::run::AGENT_TYPES_FILE);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, row(100)).unwrap();
        crate::types_snapshot::TypesSnapshot::take(&fx.repo, Some("claude"))
            .unwrap()
            .write(fx.project.agent(&id("root")).path())
            .unwrap();
        std::fs::write(&file, row(999_999)).unwrap();
        let budget = fx
            .handle
            .child_budget(&fx.repo, "capped", None, Some(&id("root")))
            .expect("the snapshot's row states a budget");
        assert_eq!(budget.tokens, Some(100), "{budget:?}");
    }

    /// **`notify/configure` turns a running supervisor's notices off and on**: a node that
    /// fails while they are off is told nobody, one that fails after they are back on is, and
    /// turning them back on does not replay what happened while they were off.
    #[test]
    fn notify_configure_turns_a_running_supervisors_notices_off_and_on() {
        let dir = scratch("owns-notify-configure");
        let record = dir.join("notices.jsonl");
        let (handle, journal) = notified(&dir, crate::notify::Backend::Record(record.clone()));
        let (out, _rx) = crate::serve::capture(ConnId(601));
        handle.hello_as_operator(out.conn());
        let configure = |enabled: bool| match handle
            .call(
                out.conn(),
                &Call::NotifyConfigure(marion_core::proto::params::NotifyConfigureParams {
                    enabled,
                }),
                &out,
            )
            .expect("notify/configure is served")
        {
            MethodResult::NotifyConfigure(r) => r.enabled,
            other => panic!("{}", other.method().as_str()),
        };
        let told = || {
            std::fs::read_to_string(&record)
                .unwrap_or_default()
                .lines()
                .count()
        };
        let failed = |agent: &str| {
            RecordKind::Exited(marion_core::journal::Exited {
                agent_id: id(agent),
                status: marion_core::contract::ExitStatus::Failed,
                exit: marion_core::contract::ProcessExit {
                    code: Some(1),
                    signal: None,
                    description: "boom".into(),
                },
            })
        };
        let step = |seq: u64, kind: RecordKind| {
            append(&journal, &line(seq, 1_000 + seq, kind));
            handle.live.refresh();
            handle.flush();
        };
        step(0, intent("quiet", None, "codex-impl", 0));
        step(1, intent("loud", None, "codex-impl", 0));
        assert!(!configure(false), "off");
        step(2, failed("quiet"));
        assert!(configure(true), "on again");
        step(
            3,
            RecordKind::Exited(marion_core::journal::Exited {
                agent_id: id("nobody"),
                status: marion_core::contract::ExitStatus::Ok,
                exit: marion_core::contract::ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: String::new(),
                },
            }),
        );
        step(4, failed("loud"));
        assert!(
            marion_testsupport::until_within(
                std::time::Duration::from_secs(10),
                std::time::Duration::from_millis(10),
                || told() >= 1
            ),
            "the failure after notices came back on was never told"
        );
        let lines = std::fs::read_to_string(&record).unwrap();
        assert_eq!(told(), 1, "only the failure while on: {lines}");
        assert!(!lines.contains("quiet"), "{lines}");
    }

    /// **Only the first claimer is shown a terminal notice**, and when it goes the next in
    /// line is shown the next one.
    #[test]
    fn only_the_head_claimer_is_sent_a_notice_and_its_departure_promotes_the_next() {
        let dir = scratch("owns-notify-claim");
        let (handle, journal) = notified(&dir, crate::notify::Backend::Terminal);
        let (first, first_rx) = crate::serve::capture(ConnId(501));
        let (second, second_rx) = crate::serve::capture(ConnId(502));
        let claim = |out: &Outbound| {
            handle.hello_as_operator(out.conn());
            match handle
                .call(
                    out.conn(),
                    &Call::NotifyClaim(marion_core::proto::params::NotifyClaimParams {}),
                    out,
                )
                .unwrap()
            {
                MethodResult::NotifyClaim(r) => r,
                other => panic!("{}", other.method().as_str()),
            }
        };
        assert!(claim(&first).head);
        assert!(!claim(&second).head);
        let notices = |rx: &crate::serve::Captured| {
            rx.try_iter()
                .filter(|b| String::from_utf8_lossy(b).contains("notify/notice"))
                .count()
        };
        let step = |seq: u64, kind: RecordKind| {
            append(&journal, &line(seq, 1_000 + seq, kind));
            handle.live.refresh();
            handle.flush();
        };
        step(0, intent("n-1", None, "codex-impl", 0));
        step(1, intent("n-2", None, "codex-impl", 0));
        step(
            2,
            state_record(
                "n-1",
                NodeState::Blocked(marion_core::node::BlockReason::Permission),
            ),
        );
        assert_eq!((notices(&first_rx), notices(&second_rx)), (1, 0));

        handle.gone(
            ConnId(501),
            &marion_core::proto::ClientGone::SocketClosed,
            &crate::serve::Departure::Eof,
        );
        step(
            3,
            state_record(
                "n-2",
                NodeState::Blocked(marion_core::node::BlockReason::Permission),
            ),
        );
        assert_eq!(notices(&second_rx), 1, "the next in line is the head now");
    }

    fn journal_len(fx: &Owning) -> usize {
        std::fs::read_to_string(fx.project.journal())
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    // ------------------------------------------------------------------------------------
    // F2: the caller states who it is and proves it, and every gated fact is derived.
    // ------------------------------------------------------------------------------------

    /// **T6's required regression: a socket spawn with a forged `SpawnCaller.agent_id` is
    /// refused.**
    ///
    /// This is the whole of F2. `serve_conn` performs no peer-credential check — `Handle::call`
    /// receives a `ConnId` and learns nothing about the process on the other end — so once
    /// `agent/spawn` is on the socket, *any* process that can `connect(2)` can name a node. The
    /// token is what separates naming from being.
    ///
    /// Both halves are asserted, because only together do they mean anything: the call is refused,
    /// **and nothing was journaled**. A refusal that arrived after the `SpawnIntent` was written
    /// would leave a node in the tree that no caller was ever entitled to create, and §5.7 would
    /// then hold the supervisor resident for it.
    #[test]
    fn a_socket_spawn_with_a_forged_caller_is_refused_and_journals_nothing() {
        let fx = owning("owns-forged", vec![intent("root", None, "claude", 0)]);
        let real = fx.handle.claim(
            &id("root"),
            Some(marion_core::contract::TaskId("t".into())),
            fx.repo.clone(),
        );
        let before = journal_len(&fx);
        // **Derived from the real token, never a fixed digit.** Spelling this as
        // `format!("{}0", &real[..real.len() - 1])` made the case *conditional on the token's
        // own last character*: one token in sixteen already ends in `0`, and for those this row
        // handed the handler the genuine token, which was accepted — a `Spawning` result where
        // a refusal was asserted, and a suite failure that came and went with the entropy.
        let real = real.expose();
        let last_byte_changed = {
            let (head, tail) = real.split_at(real.len() - 1);
            format!("{head}{}", if tail == "0" { '1' } else { '0' })
        };
        assert_ne!(
            last_byte_changed, real,
            "a forgery must differ from the real token"
        );

        for (token, why) in [
            ("not-the-token".to_string(), "a guess"),
            (String::new(), "an empty token"),
            (format!("{real}x"), "the real token with a byte appended"),
            (real[..real.len() - 1].to_string(), "a truncated prefix"),
            (
                last_byte_changed,
                "the real token with its last byte changed",
            ),
        ] {
            let e = spawn(
                &fx,
                params(
                    Some(SpawnCaller {
                        agent_id: id("root"),
                        node_token: token.into(),
                    }),
                    1,
                ),
            )
            .expect_err(&format!("{why} must not be accepted"));
            assert_eq!(e.kind(), Some(FailureKind::Refused), "{why}: {e:?}");
            assert!(
                e.message.contains("did not mint that node token"),
                "{why}: the refusal must say what was not established: {}",
                e.message
            );
        }
        assert_eq!(
            journal_len(&fx),
            before,
            "a refused spawn must journal nothing at all — not even an intent"
        );
        assert_eq!(
            fx.handle.owned_nodes(),
            1,
            "…and must not add a node to the table"
        );
    }

    /// **A node's model cannot run a program by naming it.** An authenticated node asking for
    /// `acp:<command>` that the operator never listed is refused by name, with the one line the
    /// operator would add, before anything is journaled; a refinement row still resolves past
    /// the gate.
    #[test]
    fn a_node_naming_an_unlisted_acp_command_is_refused_and_journals_nothing() {
        let fx = owning("owns-acp-cmd", vec![intent("root", None, "claude", 0)]);
        let real = fx.handle.claim(
            &id("root"),
            Some(marion_core::contract::TaskId("t".into())),
            fx.repo.clone(),
        );
        let before = journal_len(&fx);
        let caller = || {
            Some(SpawnCaller {
                agent_id: id("root"),
                node_token: real.clone(),
            })
        };
        let e = spawn(
            &fx,
            AgentSpawnParams {
                agent_type: "acp:sh -c 'echo pwned > /tmp/marion-pwned'".into(),
                ..params(caller(), 1)
            },
        )
        .expect_err("a model-named command must not run");
        assert_eq!(e.kind(), Some(FailureKind::Refused), "{e:?}");
        assert!(
            e.message.contains(crate::trust::ACP_ALLOW_FILE),
            "{}",
            e.message
        );
        assert!(e.message.contains("allow = ["), "{}", e.message);
        assert_eq!(journal_len(&fx), before, "refused before any intent");
        assert!(
            crate::trust::check_child_spawn(&AgentSpawnParams {
                agent_type: "acp:copilot".into(),
                ..params(caller(), 1)
            })
            .is_ok(),
            "a refinement row's argv is marion's"
        );
    }

    /// **A review names a node that exists and has ended, or it is refused in plain words and
    /// writes nothing** — an unknown id, and a child still running.
    #[test]
    fn a_review_of_an_unknown_or_running_node_is_refused_plainly_and_journals_nothing() {
        let fx = owning(
            "owns-review-refused",
            vec![
                intent("root", None, "claude", 0),
                intent("child", Some("root"), "codex", 1),
            ],
        );
        let token = fx.handle.claim(
            &id("root"),
            Some(marion_core::contract::TaskId("t".into())),
            fx.repo.clone(),
        );
        let before = journal_len(&fx);
        for (target, says) in [
            ("no-such-node", "no node with that id"),
            ("child", "has not ended yet"),
        ] {
            let e = spawn(
                &fx,
                AgentSpawnParams {
                    review_of: Some(id(target)),
                    ..params(
                        Some(SpawnCaller {
                            agent_id: id("root"),
                            node_token: token.expose().to_string().into(),
                        }),
                        1,
                    )
                },
            )
            .expect_err(&format!("a review of {target} must be refused"));
            assert_eq!(e.kind(), Some(FailureKind::Refused), "{target}: {e:?}");
            assert!(
                e.message
                    .contains(&format!("marion cannot review node {target}: "))
                    && e.message.contains(says),
                "{target}: {}",
                e.message
            );
        }
        assert_eq!(
            journal_len(&fx),
            before,
            "a refused review journals nothing"
        );
    }

    /// **A review token is checked like any spawn's**: a forged caller asking for a review is
    /// refused before the target is even looked up.
    #[test]
    fn a_review_asked_with_a_forged_token_is_refused() {
        let fx = owning(
            "owns-review-forged",
            vec![intent("root", None, "claude", 0)],
        );
        let _ = fx.handle.claim(
            &id("root"),
            Some(marion_core::contract::TaskId("t".into())),
            fx.repo.clone(),
        );
        let e = spawn(
            &fx,
            AgentSpawnParams {
                review_of: Some(id("root")),
                ..params(
                    Some(SpawnCaller {
                        agent_id: id("root"),
                        node_token: "not-the-token".to_string().into(),
                    }),
                    1,
                )
            },
        )
        .expect_err("a forged token must not start a review");
        assert!(
            e.message.contains("did not mint that node token"),
            "{}",
            e.message
        );
    }

    /// A caller naming a node **no supervisor ever owned** is refused by the same sentence and the
    /// same path. Distinguishing "no such node" from "wrong token" in the answer would make node
    /// existence an oracle a caller could probe with ids alone.
    #[test]
    fn a_caller_naming_a_node_this_supervisor_does_not_own_is_refused_the_same_way() {
        let fx = owning("owns-unknown", vec![intent("root", None, "claude", 0)]);
        let e = spawn(
            &fx,
            params(
                Some(SpawnCaller {
                    agent_id: id("nobody"),
                    node_token: "anything".into(),
                }),
                1,
            ),
        )
        .expect_err("an unowned node cannot spawn");
        assert_eq!(e.kind(), Some(FailureKind::Refused));
        assert!(
            e.message.contains("did not mint that node token"),
            "{}",
            e.message
        );
        // A node that *is* in the journal but was not claimed is the same case: the journal is not
        // the ownership record, the table is.
        let e = spawn(
            &fx,
            params(
                Some(SpawnCaller {
                    agent_id: id("root"),
                    node_token: "anything".into(),
                }),
                1,
            ),
        )
        .expect_err("a journalled node this supervisor never claimed cannot spawn either");
        assert!(
            e.message.contains("did not mint that node token"),
            "{}",
            e.message
        );
    }

    /// **§6.1 step 2's depth, read from the registry rather than from the call.** `SpawnCaller`
    /// carries no depth to forge, so the only way to be refused is for the supervisor to have gone
    /// and looked — and the only way to pass is to genuinely be shallow enough.
    ///
    /// The pair is the assertion: the *same* call shape is refused for a node the journal places at
    /// `max_depth` and admitted for one it places below it. A gate that read a constant, or that
    /// read anything the caller sent, could not tell the two apart.
    #[test]
    fn the_depth_gate_reads_the_callers_depth_from_the_registry() {
        let ty = agent_type::builtin("claude").unwrap();
        let deep = owning(
            "owns-depth-deep",
            vec![intent("deep", None, "claude", ty.max_depth)],
        );
        let token = deep.handle.claim(
            &id("deep"),
            Some(marion_core::contract::TaskId("t".into())),
            deep.repo.clone(),
        );
        let before = journal_len(&deep);
        let e = spawn(
            &deep,
            params(
                Some(SpawnCaller {
                    agent_id: id("deep"),
                    node_token: token,
                }),
                1,
            ),
        )
        .expect_err("a caller at max_depth may not spawn");
        assert_eq!(e.kind(), Some(FailureKind::Refused));
        assert!(
            e.message.contains("max_depth"),
            "the refusal must name the bound: {}",
            e.message
        );
        assert_eq!(
            journal_len(&deep),
            before,
            "§6.1 step 2 runs before every side effect, so a gated spawn creates nothing"
        );
    }

    /// **`live_children` inverted: counted off the registry, not off a caller's table.**
    ///
    /// `background.rs` argued the bridge's table was authoritative *by construction* because
    /// `Spawned` was written after the whole run, so a journal read could not see a child between
    /// "thread started" and "process observed". Step 1 inverted that premise — `SpawnIntent` is
    /// journaled before every side effect — and this is the assertion that the count now comes from
    /// there. Nothing in the call says how many children the caller has; the journal does.
    ///
    /// The boundary is asserted from both sides, so a count that was simply always zero (which is
    /// what the constant `LIVE_CHILDREN_OF_A_SYNCHRONOUS_CALLER` was, and never `>= 4`) fails the
    /// first half, and a count that was always saturated fails the second.
    #[test]
    fn the_concurrency_gate_counts_the_callers_children_in_the_journal() {
        let ty = agent_type::builtin("claude").unwrap();
        let max = ty.max_concurrent_children;
        let mut records = vec![intent("root", None, "claude", 0)];
        for i in 0..max {
            records.push(intent(&format!("kid{i}"), Some("root"), "claude", 1));
        }
        let fx = owning("owns-concurrency", records);
        let token = fx.handle.claim(
            &id("root"),
            Some(marion_core::contract::TaskId("t".into())),
            fx.repo.clone(),
        );
        assert_eq!(
            fx.handle.live_children_of(&id("root")),
            max,
            "the journal places exactly {max} live children under this caller"
        );
        let before = journal_len(&fx);
        let e = spawn(
            &fx,
            params(
                Some(SpawnCaller {
                    agent_id: id("root"),
                    node_token: token,
                }),
                1,
            ),
        )
        .expect_err("a caller at max_concurrent_children may not spawn");
        assert!(
            e.message.contains("max_concurrent_children"),
            "the refusal must name the bound: {}",
            e.message
        );
        assert_eq!(journal_len(&fx), before, "a gated spawn creates nothing");

        // The other side of the boundary, and the reason it is a *count* rather than a constant: a
        // child that has exited has released its slot.
        append(
            &fx.project.journal(),
            &line(
                900,
                9_000,
                RecordKind::Exited(marion_core::journal::Exited {
                    agent_id: id("kid0"),
                    status: marion_core::contract::ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "done".into(),
                    },
                }),
            ),
        );
        fx.handle.live.refresh();
        assert_eq!(
            fx.handle.live_children_of(&id("root")),
            max - 1,
            "a terminal child no longer occupies a §3.1 concurrency slot"
        );
    }

    /// A root spawn that states a repository, which is the well-formed shape for the case.
    fn root_params(repo: &Path, secs: u64) -> AgentSpawnParams {
        AgentSpawnParams {
            native_launch: None,
            repo: Some(repo.to_path_buf()),
            // A root has no contract, so no scope (`check_root_contract_fields`).
            writable_scope: vec![],
            ..params(None, secs)
        }
    }

    #[cfg(unix)]
    fn opaque(bytes: &[u8]) -> OpaqueOsValueV1 {
        use std::os::unix::ffi::OsStrExt;

        OpaqueOsValueV1::from_os_str(std::ffi::OsStr::from_bytes(bytes))
            .expect("Unix preserves native launch bytes")
    }

    #[cfg(unix)]
    fn native_context(repo: &Path) -> NativeLaunchContextV1 {
        NativeLaunchContextV1::new(
            opaque(b"/native/program-\xff"),
            vec![opaque(b"--opaque=\xfe")],
            OpaqueOsValueV1::from_os_str(repo.as_os_str())
                .expect("the repository path is byte-exact on Unix"),
            vec![NativeEnvVarV1 {
                name: opaque(b"NATIVE_MARKER"),
                value: opaque(b"value-\xfd"),
            }],
            TerminalGeometryV1 {
                cols: 137,
                rows: 43,
                xpixel: 9,
                ypixel: 11,
            },
        )
    }

    #[cfg(unix)]
    fn native_context_v2(repo: &Path) -> NativeLaunchContextV2 {
        let context = native_context(repo);
        NativeLaunchContextV2::new(
            "atlas".into(),
            context.program,
            context.argv,
            context.cwd,
            context.env,
            context.geometry,
        )
    }

    /// The early pure boundary owns only child/native pairing; root binding happens after peer
    /// authentication and owns selector, platform, transport, and executable decisions.
    #[cfg(unix)]
    #[test]
    fn the_early_native_launch_boundary_owns_only_child_pairing() {
        let fx = owning("owns-native-boundary", vec![]);
        let caller = SpawnCaller {
            agent_id: id("caller"),
            node_token: "token".into(),
        };
        let context = NativeLaunchContext::V1(native_context(&fx.repo));

        assert_eq!(validate_native_launch_boundary(None, None), Ok(()));
        assert_eq!(validate_native_launch_boundary(Some(&caller), None), Ok(()));
        assert_eq!(
            validate_native_launch_boundary(Some(&caller), Some(&context)),
            Err(NativeLaunchGateError::ChildMisuse)
        );
        assert_eq!(
            validate_native_launch_boundary(None, Some(&context)),
            Ok(())
        );
    }

    /// Native process state belongs only to the root request that originated at the CLI.
    /// A child carrying it is a category error, before token lookup and before every write.
    #[cfg(unix)]
    #[test]
    fn a_child_request_carrying_native_launch_is_refused_before_every_side_effect() {
        let fx = owning("owns-native-child", vec![]);
        let before = journal_len(&fx);
        let mut p = params(
            Some(SpawnCaller {
                agent_id: id("not-owned"),
                node_token: "not-a-token".into(),
            }),
            1,
        );
        p.native_launch = Some(Box::new(NativeLaunchContext::V1(native_context(&fx.repo))));

        let e = spawn(&fx, p).expect_err("native launch state is root-only");

        assert_eq!(e.kind(), Some(FailureKind::Refused), "{e}");
        assert!(
            e.message
                .contains("native launch context belongs only on a root"),
            "the refusal must name child misuse: {}",
            e.message
        );
        assert_eq!(journal_len(&fx), before, "the refusal journals nothing");
        assert_eq!(fx.handle.owned_nodes(), 0, "the refusal claims no node");
    }

    /// V1 cannot name the facade that owns its program, so it stops before every launch effect.
    #[cfg(unix)]
    #[test]
    fn a_v1_root_native_launch_is_refused_because_its_selector_is_missing() {
        let fx = owning("owns-native-root", vec![]);
        let mut p = root_params(&fx.repo, 1);
        p.native_launch = Some(Box::new(NativeLaunchContext::V1(native_context(&fx.repo))));

        let e = spawn(&fx, p).expect_err("V1 cannot be bound without a selector");

        assert_eq!(e.kind(), Some(FailureKind::Refused), "{e}");
        assert!(
            e.message.contains("V1 carries no facade selector"),
            "the refusal must name the missing selector: {}",
            e.message
        );
        assert_eq!(journal_len(&fx), 0, "the binding gate journals nothing");
        assert_eq!(
            fx.handle.owned_nodes(),
            0,
            "the binding gate claims no node"
        );
    }

    /// Ordinary root RPC cannot present the native-bootstrap authority required by V2.
    #[cfg(unix)]
    #[test]
    fn a_v2_root_native_launch_requires_the_native_bootstrap() {
        let fx = owning("owns-native-v2-root", vec![]);
        let mut p = root_params(&fx.repo, 1);
        p.native_launch = Some(Box::new(NativeLaunchContext::V2(native_context_v2(
            &fx.repo,
        ))));

        let e = spawn(&fx, p).expect_err("ordinary root RPC cannot authorize raw V2");

        assert_eq!(e.kind(), Some(FailureKind::Refused), "{e}");
        assert!(
            e.message
                .contains("raw V2 native launch requires the native bootstrap"),
            "the refusal must name missing native-bootstrap authority: {}",
            e.message
        );
        assert_eq!(journal_len(&fx), 0, "the binding gate journals nothing");
        assert_eq!(
            fx.handle.owned_nodes(),
            0,
            "the binding gate claims no node"
        );
    }

    /// A raw V2 frame cannot become bound even if its executable-shaped fields are valid.
    #[cfg(unix)]
    #[test]
    fn raw_v2_handler_validation_has_no_bound_success_arm() {
        let work = scratch("native-handler-bound-refusal");
        let bin = work.join("bin");
        std::fs::create_dir(&bin).expect("the fixture bin exists");
        let executable = bin.join("atlas-cli");
        std::fs::write(&executable, b"fixture executable\n").expect("the executable is written");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("the executable is executable");
        let context = NativeLaunchContext::V2(NativeLaunchContextV2::new(
            "atlas".into(),
            OpaqueOsValueV1::from_os_str(executable.as_os_str()).unwrap(),
            vec![],
            OpaqueOsValueV1::from_os_str(work.as_os_str()).unwrap(),
            vec![NativeEnvVarV1 {
                name: opaque(b"PATH"),
                value: OpaqueOsValueV1::from_os_str(bin.as_os_str()).unwrap(),
            }],
            TerminalGeometryV1 {
                cols: 80,
                rows: 24,
                xpixel: 0,
                ypixel: 0,
            },
        ));

        let error = native_launch_refusal(&context);

        assert_eq!(error.kind(), Some(FailureKind::Refused));
        assert!(
            error
                .message
                .contains("raw V2 native launch requires the native bootstrap"),
            "raw V2 must stop before registry or managed-launch selection: {}",
            error.message
        );
    }

    /// **A root's `--timeout` reaches the spec that both enforces and journals it.**
    ///
    /// `marion run --timeout 300` states an `Option<u64>` on the wire and the supervisor
    /// resolves it (`root::blocked_bound_secs`). Resolved *into the spec* rather than beside
    /// it, because `root::prepare` is what writes the node's `SpawnIntent`: a bound held
    /// somewhere the intent cannot see is a bound no reader can report, which is how every node
    /// in `marion tree` came to read 900 s. A run that states nothing keeps the type's.
    #[test]
    fn a_root_spec_carries_the_bound_the_operator_asked_for() {
        let fx = owning("owns-root-bound", vec![]);
        let env = fx
            .handle
            .spawn_env
            .as_ref()
            .expect("an owning handle has a spawn environment");
        let ty = agent_type::builtin("claude").expect("the fixture type exists");

        let asked = root_spec_from_spawn(&root_params(&fx.repo, 300), fx.repo.clone(), env, &ty);
        assert_eq!(asked.bound_secs, 300);

        let mut p = root_params(&fx.repo, 300);
        p.timeout_secs = None;
        let silent = root_spec_from_spawn(&p, fx.repo.clone(), env, &ty);
        assert_eq!(
            silent.bound_secs,
            ty.timeout.0.as_secs(),
            "a run that names no bound gets §3.1's, which is the type's"
        );
        assert_ne!(
            asked.bound_secs, silent.bound_secs,
            "and the two are distinguishable, or the first assertion proves nothing"
        );
    }

    /// The production `RootSpec` constructor is the preparatory threading proof: byte-exact
    /// context survives that boundary even though the readiness gate keeps production from
    /// reaching it today.
    #[cfg(unix)]
    #[test]
    fn root_spec_construction_preserves_native_launch_byte_for_byte() {
        let fx = owning("owns-native-spec", vec![]);
        let context = native_context(&fx.repo);
        let expected = serde_json::to_vec(&context).expect("the context serializes");
        let mut p = root_params(&fx.repo, 1);
        p.native_launch = Some(Box::new(NativeLaunchContext::V1(context)));
        let env = fx
            .handle
            .spawn_env
            .as_ref()
            .expect("an owning handle has a spawn environment");
        let ty = agent_type::builtin(&p.agent_type).expect("the fixture type exists");

        let spec = root_spec_from_spawn(&p, fx.repo.clone(), env, &ty);
        let carried = spec
            .native_launch
            .as_ref()
            .expect("the root spec carries native context");
        let actual = serde_json::to_vec(carried).expect("the carried context serializes");

        assert!(
            actual == expected,
            "RootSpec must preserve every opaque native-context byte"
        );
    }

    /// **§11 item 28 step 6, at the handler: a client creating a root is served.**
    ///
    /// This test replaces `a_client_creating_a_root_over_the_socket_is_refused_naming_the_step_
    /// that_serves_it`, which asserted the opposite and was the honest pin while root creation
    /// was owed. Renamed rather than deleted, because the *name* is what a reader greps for and
    /// a stale one asserting a served path is refused would be a lie with a green tick next to
    /// it.
    ///
    /// What it can assert without launching a harness is that the frame **gets past every
    /// refusal that used to stop it** and is then judged on its own merits: the agent type is
    /// one no build has, which is the same lookup `root::prepare` performs, and nothing is
    /// journaled because nothing was minted. `client_run.rs` is where a root that really starts
    /// is measured, through `marion run` and a real provider.
    #[test]
    fn a_client_creating_a_root_reaches_the_launcher_rather_than_a_step_that_would_serve_it() {
        let fx = owning("owns-root", vec![]);
        let e = spawn(
            &fx,
            AgentSpawnParams {
                native_launch: None,
                agent_type: "no-such-agent-type".into(),
                ..root_params(&fx.repo, 1)
            },
        )
        .expect_err("no build has that agent type");
        assert!(
            !e.message.contains("step 6"),
            "root creation is served; a refusal naming the step that would serve it is a \
             revert: {}",
            e.message
        );
        assert_eq!(e.kind(), Some(FailureKind::Refused), "{e:?}");
        assert!(
            e.message
                .contains("the agent type is not a built-in and not a row"),
            "the frame reached the root launcher: {}",
            e.message
        );
        assert_eq!(journal_len(&fx), 0, "and nothing was created");
        assert_eq!(fx.handle.owned_nodes(), 0, "…and no node was claimed");
    }

    /// **A node thread that panics after its claim still reaches a terminal outcome, and the
    /// supervisor can still exit.**
    ///
    /// Neither thread body had a `catch_unwind`. A panic after `SpawnObserver::identified`
    /// claimed the node and before `mark_finished` left `NodeHandle::outcome` `None` **for
    /// ever**, and `None` means *still running*: `running_nodes` counted it, so
    /// `idle_exit_eligible` refused, so `join_finished_nodes` never reaped the thread, and the
    /// supervisor could not exit for the rest of its life. A permanent phantom node, from one
    /// unwind.
    ///
    /// The panic is **real and injected in the window that matters** — inside `identified`,
    /// after the claim — rather than simulated by writing an outcome by hand, because what is
    /// being measured is the unwind path itself.
    ///
    /// **Bounded without a timeout.** Nothing here polls or sleeps: the thread sends
    /// `Progress::Finished` strictly after `mark_finished`, and `spawn_root` returns on
    /// receiving it, so the outcome is filed before this call returns. With the `catch_unwind`
    /// removed the thread dies instead, dropping its sender, and the receive fails as
    /// *disconnected* rather than waiting out `LAUNCH_BOUND` — so the mutation fails this test
    /// in milliseconds, on the assertion, which is what a mutation has to do to count.
    #[test]
    fn a_node_thread_that_panics_after_its_claim_still_reaches_a_terminal_outcome() {
        let fx = owning("owns-panic", vec![]);
        {
            let _armed = panic_after_claim::arm(&fx.repo);
            // A real agent type, so the frame reaches `prepare_watched` and the claim inside
            // it. The panic then fires before `prepare_watched`'s first side effect.
            spawn(&fx, root_params(&fx.repo, 1))
                .expect_err("the node's thread panicked, so no process ever existed");
        }

        let claimed: Vec<AgentId> = lock(&fx.handle.nodes).keys().cloned().collect();
        assert_eq!(
            claimed.len(),
            1,
            "the premise: the panic came *after* the claim, so there is a node to strand"
        );
        let agent_id = &claimed[0];

        assert_eq!(
            fx.handle.running_nodes(),
            0,
            "**terminal.** A node whose thread panicked is not running, and an `outcome` left \
             `None` here is the phantom: nothing in this process ever sets it afterwards"
        );
        let why = fx
            .handle
            .owned_failure(agent_id)
            .expect("a panicked node has a failure to report");
        assert!(
            why.contains("panicked"),
            "and the journal-facing sentence says what happened rather than inventing a \
             clean exit: {why}"
        );

        assert!(
            fx.handle.idle_exit_eligible(),
            "**§5.7.** With no clients and no running node the supervisor is eligible to \
             leave; this is the predicate the phantom held false for ever"
        );
        assert!(
            fx.handle.begin_idle_exit(),
            "and it really exits — `begin_idle_exit` joins the finished threads first, which \
             is the step `join_finished_nodes` could never reach for an unreaped panic"
        );
    }

    /// **The same, on the child thread**, so the other `caught` call site is measured and not
    /// merely compiled.
    ///
    /// A child needs a caller holding a real token, and that caller is itself a claimed node
    /// this fixture never finishes — so the assertions here are about *this* node rather than
    /// about `running_nodes` or `idle_exit_eligible`, which the root test above owns.
    #[test]
    fn a_child_threads_panic_is_that_childs_own_terminal_outcome() {
        let fx = owning("owns-panic-child", vec![intent("root", None, "claude", 0)]);
        let token = fx.handle.claim(
            &id("root"),
            Some(marion_core::contract::TaskId("t".into())),
            fx.repo.clone(),
        );
        {
            let _armed = panic_after_claim::arm(&fx.repo);
            spawn(
                &fx,
                params(
                    Some(SpawnCaller {
                        agent_id: id("root"),
                        node_token: token,
                    }),
                    1,
                ),
            )
            .expect_err("the child's thread panicked, so no process ever existed");
        }

        let child = lock(&fx.handle.nodes)
            .keys()
            .find(|k| **k != id("root"))
            .cloned()
            .expect("the panic came after the claim, so the child is in the table");
        assert_eq!(
            fx.handle.owned_running(&child),
            Some(false),
            "**terminal.** Without the `catch_unwind` this stays `Some(true)` for the life of \
             the supervisor, and §5.7 never lets it exit"
        );
        let why = fx
            .handle
            .owned_failure(&child)
            .expect("a panicked child has a failure to report");
        assert!(
            why.contains("panicked") && why.contains("no contract"),
            "and it is `SpawnError::Panicked`'s written sentence, which says there is no \
             contract rather than leaving a caller to wait for one: {why}"
        );
    }

    /// The child half of the same mechanism, without a launch.
    ///
    /// The injection above exercises `caught` through the **root** arm, because a root needs no
    /// caller and so needs no token, no worktree and no harness. This pins the other
    /// vocabulary directly: §9 gives a root and a child different results, and `caught` takes
    /// the spelling from its caller precisely so neither gets the other's.
    #[test]
    fn a_panic_becomes_each_kind_of_nodes_own_way_of_saying_it_failed() {
        let child = caught("claude", spawn_panicked, || -> Result<(), _> {
            panic!("inside the child's thread")
        })
        .expect_err("a panic is not a success");
        assert!(
            matches!(child, crate::spawn::SpawnError::Panicked(ref t) if t == "claude"),
            "a child's panic is `SpawnError::Panicked` — the variant that was documented for \
             exactly this and constructed nowhere until now: {child:?}"
        );
        assert!(
            child.to_string().contains("no contract"),
            "and it carries the written sentence, which points at the journal: {child}"
        );

        let root = caught("codex", root_panicked, || -> Result<(), _> {
            panic!("inside the root's thread")
        })
        .expect_err("a panic is not a success");
        assert!(
            root.contains("codex") && root.contains("panicked"),
            "a root has no `TaskContract` and so no `SpawnError`; its outcome is marion's own \
             sentence: {root}"
        );

        assert_eq!(
            caught(
                "claude",
                spawn_panicked,
                || Ok::<_, crate::spawn::SpawnError>(7)
            )
            .ok(),
            Some(7),
            "and a body that does not panic is passed through untouched"
        );
    }

    /// **The `no_change_record` half of the root-only pairing.**
    ///
    /// §9's change record exists because a root runs in the operator's own checkout. A child
    /// runs in a worktree marion made, so the field would be an accept-and-ignore there — and
    /// §11 item 23's whole rule is that a caller told nothing has been told their choice was
    /// honoured.
    #[test]
    fn a_caller_that_states_no_change_record_is_refused_by_name() {
        let fx = owning("owns-ncr", vec![intent("root", None, "claude", 0)]);
        let token = fx.handle.claim(
            &id("root"),
            Some(marion_core::contract::TaskId("t".into())),
            fx.repo.clone(),
        );
        let before = journal_len(&fx);
        let e = spawn(
            &fx,
            AgentSpawnParams {
                native_launch: None,
                // `false` and not `true`: the refusal is for *stating* it, so a test that sent
                // the interesting value would pass against a build that only refused `true`.
                no_change_record: Some(false),
                pane: None,
                isolation: None,
                allow_concurrent_writes: None,
                ..params(
                    Some(SpawnCaller {
                        agent_id: id("root"),
                        node_token: token,
                    }),
                    1,
                )
            },
        )
        .expect_err("a child has no change record to decline");
        assert_eq!(e.kind(), Some(FailureKind::Refused));
        assert!(
            e.message.contains("must not state `no_change_record`"),
            "the refusal must name the field: {}",
            e.message
        );
        assert_eq!(journal_len(&fx), before, "and nothing was created");
    }

    /// **An agent cannot lift its own tree's sandbox by asking**: `wider_children` on a
    /// spawn with a caller is refused by name, before anything exists, whatever its value.
    #[test]
    fn a_caller_that_asks_for_wider_children_is_refused_by_name() {
        let fx = owning("owns-wider", vec![intent("root", None, "codex", 0)]);
        let token = fx.handle.claim(
            &id("root"),
            Some(marion_core::contract::TaskId("t".into())),
            fx.repo.clone(),
        );
        let before = journal_len(&fx);
        let e = spawn(
            &fx,
            AgentSpawnParams {
                wider_children: Some(false),
                ..params(
                    Some(SpawnCaller {
                        agent_id: id("root"),
                        node_token: token,
                    }),
                    1,
                )
            },
        )
        .expect_err("only the operator can opt a tree in");
        assert_eq!(e.kind(), Some(FailureKind::Refused));
        assert!(
            e.message.contains("An agent cannot ask for it"),
            "{}",
            e.message
        );
        assert_eq!(journal_len(&fx), before, "and nothing was created");
    }

    /// A call as connection `conn` makes it, through the one dispatch every socket call takes.
    fn call_on(fx: &Owning, conn: ConnId, call: Call) -> Result<MethodResult, RpcError> {
        fx.handle.call(conn, &call, &crate::serve::sink(conn))
    }

    fn hello_operator(fx: &Owning, conn: ConnId) -> Result<MethodResult, RpcError> {
        let state = &fx.handle.spawn_env.as_ref().unwrap().state;
        let key = crate::operator_key::read(state)
            .unwrap()
            .expect("minted at boot");
        call_on(
            fx,
            conn,
            Call::SessionHello(marion_core::proto::params::SessionHelloParams {
                operator: Some(key),
                node: None,
            }),
        )
    }

    fn quit_detach() -> Call {
        Call::SessionQuit(marion_core::proto::params::SessionQuitParams {
            disposition: marion_core::proto::QuitDisposition::DetachAll,
        })
    }

    /// **A connection that has not said who it speaks for may call nothing but `session/hello`**
    /// — not even a read: node output can be sensitive, and the uid on the socket is shared by
    /// every process the operator runs, a node's shell included.
    #[test]
    fn a_connection_that_names_nobody_is_refused_every_call() {
        let fx = owning("owns-hello-none", vec![intent("root", None, "claude", 0)]);
        let conn = ConnId(71);
        for call in [
            Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
            Call::NodeGet(marion_core::proto::params::NodeGetParams::of(id("root"))),
            quit_detach(),
        ] {
            let e = call_on(&fx, conn, call).expect_err("an anonymous call is refused");
            assert!(e.message.contains("session/hello"), "{}", e.message);
        }
    }

    /// **The operator's key opens every method; a wrong key opens none.** A connection says who
    /// it is once.
    #[test]
    fn the_operators_key_opens_every_method_and_a_wrong_one_none() {
        let fx = owning(
            "owns-hello-operator",
            vec![intent("root", None, "claude", 0)],
        );
        let wrong = call_on(
            &fx,
            ConnId(72),
            Call::SessionHello(marion_core::proto::params::SessionHelloParams {
                operator: Some(Secret::new("0".repeat(64))),
                node: None,
            }),
        );
        assert!(wrong.is_err(), "a guessed key is refused");
        assert!(call_on(&fx, ConnId(72), quit_detach()).is_err());

        assert!(matches!(
            hello_operator(&fx, ConnId(73)),
            Ok(MethodResult::SessionHello(_))
        ));
        assert!(call_on(&fx, ConnId(73), quit_detach()).is_ok());
        assert!(
            hello_operator(&fx, ConnId(73)).is_err(),
            "a connection says who it speaks for once"
        );
    }

    /// **The operator's key is refused from inside a node's process tree** — defense in depth
    /// for a key a same-uid process could read: a node's shell that lifted it still descends
    /// from the node.
    #[test]
    fn the_operators_key_is_refused_from_a_process_below_a_live_node() {
        let fx = owning("owns-hello-below", vec![intent("root", None, "claude", 0)]);
        fx.handle.claim(&id("root"), None, fx.repo.clone());
        let parent = std::os::unix::process::parent_id() as i32;
        fx.handle.mark_started(&id("root"), parent);
        let conn = ConnId(76);
        let out = crate::serve::sink_from(conn, std::process::id());
        let e = fx
            .handle
            .call(conn, &Call::SessionHello(fx.handle.operator_hello()), &out)
            .expect_err("a process below a live node is not the operator");
        assert!(e.message.contains("root"), "names the node: {}", e.message);
        assert!(
            call_on(&fx, conn, quit_detach()).is_err(),
            "and the connection speaks for nobody"
        );
    }

    /// **A node's token makes a connection that node**: it reads, and acts about itself and the
    /// nodes below it, and nothing the operator alone may do — no quit, no root.
    #[test]
    fn a_node_connection_acts_only_about_itself_and_the_nodes_below_it() {
        let fx = owning(
            "owns-hello-node",
            vec![
                intent("root", None, "claude", 0),
                intent("sibling", None, "claude", 0),
            ],
        );
        let token = fx.handle.claim(
            &id("root"),
            Some(marion_core::contract::TaskId("t".into())),
            fx.repo.clone(),
        );
        let conn = ConnId(74);
        let said = call_on(
            &fx,
            conn,
            Call::SessionHello(marion_core::proto::params::SessionHelloParams {
                operator: None,
                node: Some(SpawnCaller {
                    agent_id: id("root"),
                    node_token: token.clone(),
                }),
            }),
        );
        assert!(
            matches!(said, Ok(MethodResult::SessionHello(_))),
            "{said:?}"
        );
        assert!(
            call_on(
                &fx,
                conn,
                Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {})
            )
            .is_ok()
        );
        let quit = call_on(&fx, conn, quit_detach()).expect_err("a node cannot quit marion");
        assert!(quit.message.contains("operator"), "{}", quit.message);
        let kill = call_on(
            &fx,
            conn,
            Call::NodeKill(marion_core::proto::params::NodeKillParams {
                agent_id: id("sibling"),
            }),
        )
        .expect_err("a node cannot end a node outside its subtree");
        assert!(kill.message.contains("below"), "{}", kill.message);
        let cancel = call_on(
            &fx,
            conn,
            Call::NodeCancel(marion_core::proto::params::NodeCancelParams {
                agent_id: id("sibling"),
                caller: Some(SpawnCaller {
                    agent_id: id("root"),
                    node_token: token.clone(),
                }),
            }),
        )
        .expect_err("a node cannot cancel a node outside its subtree");
        assert!(cancel.message.contains("below"), "{}", cancel.message);
        let root = call_on(&fx, conn, Call::AgentSpawn(params(None, 1)))
            .expect_err("a node cannot start a root");
        assert!(root.message.contains("operator"), "{}", root.message);
        let notices = call_on(
            &fx,
            conn,
            Call::NotifyConfigure(marion_core::proto::params::NotifyConfigureParams {
                enabled: false,
            }),
        )
        .expect_err("a node cannot silence the operator's notices");
        assert!(notices.message.contains("operator"), "{}", notices.message);
        let workflow = call_on(
            &fx,
            conn,
            Call::WorkflowRun(marion_core::proto::params::WorkflowRunParams {
                name: "ship".into(),
                inputs: Default::default(),
                repo: fx.repo.clone(),
            }),
        )
        .expect_err("a node cannot start the operator's contracted nodes");
        assert!(
            workflow.message.contains("operator"),
            "{}",
            workflow.message
        );
        let stop = call_on(
            &fx,
            conn,
            Call::WorkflowCancel(marion_core::proto::params::WorkflowCancelParams {
                wf_id: marion_core::workflow::WorkflowId("w-1".into()),
                force: true,
            }),
        )
        .expect_err("a node cannot stop the operator's workflow run");
        assert!(stop.message.contains("operator"), "{}", stop.message);

        let forged = call_on(
            &fx,
            ConnId(75),
            Call::SessionHello(marion_core::proto::params::SessionHelloParams {
                operator: None,
                node: Some(SpawnCaller {
                    agent_id: id("root"),
                    node_token: Secret::new("f".repeat(64)),
                }),
            }),
        );
        assert!(forged.is_err(), "a forged token names nobody");
    }

    /// **A root that states a check is refused by name, before anything exists.** A root has no
    /// contract (§9), so `verification` or `writable_scope` on it would be checks nobody ran;
    /// each is refused alone, and a child keeps both.
    ///
    /// Mutation: drop the `check_root_contract_fields` call and the root is created with its
    /// verification dropped.
    #[test]
    fn a_root_that_states_verification_or_scope_is_refused_by_name() {
        let fx = owning("owns-root-contract", vec![]);
        let root = |p: AgentSpawnParams| AgentSpawnParams {
            repo: Some(fx.repo.clone()),
            writable_scope: vec![],
            ..p
        };
        let before = journal_len(&fx);
        for (field, p) in [
            (
                "verification",
                AgentSpawnParams {
                    verification: vec!["cargo test".into()],
                    ..root(params(None, 1))
                },
            ),
            (
                "writable_scope",
                AgentSpawnParams {
                    writable_scope: vec!["src/**".into()],
                    ..root(params(None, 1))
                },
            ),
        ] {
            let e = spawn(&fx, p).expect_err("a root has no contract to check");
            assert_eq!(e.kind(), Some(FailureKind::Refused), "{field}");
            assert!(
                e.message.contains(&format!("`{field}`")),
                "the refusal names {field}: {}",
                e.message
            );
        }
        assert_eq!(journal_len(&fx), before, "and nothing was created");
        assert!(
            RegistryHandle::check_root_contract_fields(&params(
                Some(SpawnCaller {
                    agent_id: id("root"),
                    node_token: "t".into(),
                }),
                1,
            ))
            .is_ok(),
            "a child states them freely"
        );
    }

    /// **Open question 3, decided and pinned: root creation is authorized by filesystem
    /// permission on the socket, checked against peer credentials.**
    ///
    /// Asserted against the predicate rather than through a connection, and that is a stated
    /// limitation rather than a shortcut: making a real peer of another uid needs a second
    /// account or a setuid helper, neither of which a `cargo test` may assume. What the
    /// predicate *is* reached through in production is one line in `Handle::call`
    /// (`out.peer()`), and `Peer` is read there and nowhere else.
    ///
    /// The `Unknown` row is the load-bearing one: `getpeereid` can fail, and a check that could
    /// not be made must refuse rather than pass. A build that spelled this
    /// `matches!(peer, Peer::Uid(u) if u != own)` would accept every unreadable peer.
    #[test]
    fn root_creation_is_refused_to_a_peer_that_is_not_this_supervisors_own_user() {
        let own = crate::socket::own_uid();
        assert!(
            root_spawn_authorized(Peer::Uid(own)).is_ok(),
            "the supervisor's own user is who marion serves"
        );

        let e = root_spawn_authorized(Peer::Uid(own.wrapping_add(1)))
            .expect_err("another user's process may not start work here");
        assert_eq!(e.kind(), Some(FailureKind::Refused));
        assert!(
            e.message.contains("filesystem permission on the socket"),
            "the refusal must say what does authorize it: {}",
            e.message
        );

        let e = root_spawn_authorized(Peer::Unknown)
            .expect_err("a check that could not be made is not a check that passed");
        assert_eq!(e.kind(), Some(FailureKind::Refused));
        assert!(
            e.message
                .contains("could not read this connection's peer credentials"),
            "{}",
            e.message
        );
    }

    /// Native root state is still attacker-controlled until the socket peer is authenticated.
    /// An unauthorized peer learns only that its credentials were refused: platform support
    /// and transport readiness are facts marion reveals after that boundary, never before it.
    #[cfg(unix)]
    #[test]
    fn an_unauthorized_root_native_request_gets_the_credential_refusal_first() {
        let fx = owning("owns-native-peer", vec![]);
        let mut p = root_params(&fx.repo, 1);
        p.native_launch = Some(Box::new(NativeLaunchContext::V1(native_context(&fx.repo))));

        let e = fx
            .handle
            .agent_spawn(&p, Peer::Unknown)
            .expect_err("an unreadable peer may not create a native root");

        assert_eq!(e.kind(), Some(FailureKind::Refused), "{e}");
        assert!(
            e.message
                .contains("could not read this connection's peer credentials"),
            "credential refusal must precede platform/readiness disclosure: {}",
            e.message
        );
        assert!(
            !e.message.contains("native facade")
                && !e.message.contains("byte-exact operating-system")
                && !e.message.contains("V1 carries no facade selector"),
            "an unauthorized peer learned native support state: {}",
            e.message
        );
        assert_eq!(journal_len(&fx), 0, "the refusal journals nothing");
        assert_eq!(fx.handle.owned_nodes(), 0, "the refusal claims no node");
    }

    /// **Neither the peer check nor the operator's connection stands in for a node token.**
    ///
    /// A caller that *does* name a node still has to prove it, and this asserts the two paths
    /// do not blur: the peer check never stands in for `resolve_caller`. Both calls below come
    /// from the same connection and the same uid; one is served, the other is refused for a
    /// reason that has nothing to do with the user.
    #[test]
    fn the_peer_check_does_not_stand_in_for_a_node_token() {
        let fx = owning(
            "owns-peer-not-token",
            vec![intent("root", None, "claude", 0)],
        );
        let e = spawn(
            &fx,
            params(
                Some(SpawnCaller {
                    agent_id: id("root"),
                    node_token: "not-the-token".into(),
                }),
                1,
            ),
        )
        .expect_err("§5.4 binds a capability to an AgentId, and this connection has none");
        assert!(
            e.message.contains("did not mint that node token"),
            "peer credentials answer which *user*; the token answers which *node*, and this \
             refusal must be the second: {}",
            e.message
        );
    }

    /// **A caller does not get to say which tree its child branches from.**
    ///
    /// Same argument as `SpawnCaller` carrying no `agent_type` and no `depth`: every gated fact
    /// about a caller is derived from what the supervisor minted. A node that could name its own
    /// repository could branch its children off a tree its parent never entitled it to touch —
    /// and, since one supervisor serves a repository *and every linked worktree of it*, the
    /// trees within reach of a forged value are exactly the ones an operator is working in.
    ///
    /// Refused rather than ignored, for §11 item 23's rule: a caller that states a repository,
    /// receives no error and is quietly given a different one has been told nothing.
    #[test]
    fn a_caller_that_states_its_own_repository_is_refused_by_name() {
        let fx = owning("owns-stated-repo", vec![intent("root", None, "claude", 0)]);
        let token = fx.handle.claim(
            &id("root"),
            Some(marion_core::contract::TaskId("t".into())),
            fx.repo.clone(),
        );
        let before = journal_len(&fx);
        let e = spawn(
            &fx,
            AgentSpawnParams {
                native_launch: None,
                // The *real* tree, not an implausible one: the refusal must not depend on the
                // value being wrong. Stating it at all is the error.
                repo: Some(fx.repo.clone()),
                ..params(
                    Some(SpawnCaller {
                        agent_id: id("root"),
                        node_token: token,
                    }),
                    1,
                )
            },
        )
        .expect_err("a caller may not state its own repository");
        assert_eq!(e.kind(), Some(FailureKind::Refused));
        assert!(
            e.message.contains("must not state a `repo`"),
            "the refusal must name the field and the pairing: {}",
            e.message
        );
        assert_eq!(journal_len(&fx), before, "and nothing was created");
    }

    /// **The other half of the pairing: a root spawn must say which tree it is of.**
    ///
    /// There is no derivation available. §2 keys this supervisor on `git rev-parse
    /// --git-common-dir`, so `<state>/<project-hash>` names a repository and every linked
    /// worktree of it at once, and defaulting to the project root would branch a feature
    /// worktree's children off the main tree's HEAD.
    ///
    /// Asserted through the `Refused` **kind** as well as the sentence, because the neighbouring
    /// root refusal is `Unimplemented`: a build that answered "step 6" to a malformed frame
    /// would be hiding a client's mistake behind marion's own.
    #[test]
    fn a_root_spawn_that_states_no_repository_is_refused_by_name() {
        let fx = owning("owns-no-repo", vec![]);
        let e = spawn(&fx, params(None, 1))
            .expect_err("a root spawn with no repository cannot be served");
        assert_eq!(
            e.kind(),
            Some(FailureKind::Refused),
            "not `Unimplemented`: this is the client's frame being wrong, not marion's build \
             being incomplete — {e:?}"
        );
        assert!(
            e.message.contains("only the client knows which tree"),
            "the refusal must say why nothing here could supply it: {}",
            e.message
        );
        assert_eq!(journal_len(&fx), 0, "and nothing was created");
    }

    /// **A worktree child's children branch from its worktree**, not from the tree its parent
    /// named (live smoke s2, 2026-09-27: every grandchild was cut from the root's HEAD and
    /// tested none of its parent's code). Choosing the workspace re-points the child's entry;
    /// a `shared-cwd` child keeps the tree it inherited, which is where it runs.
    #[test]
    fn a_worktree_childs_own_children_branch_from_its_worktree() {
        let fx = owning("owns-child-tree", vec![]);
        let (progress, _rx) = std::sync::mpsc::channel();
        let owner = NodeOwner {
            handle: Arc::clone(&fx.handle),
            task_id: Some(marion_core::contract::TaskId("t-child".into())),
            repo: fx.repo.clone(),
            tx: progress,
            identified: Mutex::new(None),
            announce_to: None,
            owes: Default::default(),
        };
        let child = id("child");
        let _token = <NodeOwner as crate::run::SpawnObserver>::identified(&owner, &child);
        let tree_of = |n: &AgentId| lock(&fx.handle.nodes).get(n).map(|node| node.repo.clone());
        assert_eq!(tree_of(&child), Some(fx.repo.clone()), "inherited at claim");

        let shared = marion_core::contract::Workspace::SharedCwd {
            path: fx.repo.clone(),
        };
        crate::run::SpawnObserver::workspace_chosen(&owner, &child, &shared);
        assert_eq!(tree_of(&child), Some(fx.repo.clone()));

        let wt = fx.project.agent(&child).worktree();
        let worktree = marion_core::contract::Workspace::Worktree {
            path: wt.clone(),
            branch: "marion/t-child".into(),
        };
        crate::run::SpawnObserver::workspace_chosen(&owner, &child, &worktree);
        assert_eq!(
            tree_of(&child),
            Some(wt),
            "its children branch from its worktree"
        );
    }

    /// **The load-bearing one: two roots in two linked worktrees of one repository, whose
    /// children branch from different HEADs.**
    ///
    /// This is the whole reason the repository left `run::Env`. §2 keys a supervisor on `git
    /// rev-parse --git-common-dir`, so both trees below resolve to **one** project, one journal
    /// and one supervisor — that is asserted here rather than assumed, because if it were false
    /// the rest of the test would be measuring two supervisors and proving nothing. And
    /// `spawn::make_worktree` runs `git -C <repo> rev-parse HEAD`, so the tree each child
    /// branches from is a property of *its own* root, not of the supervisor they share.
    ///
    /// The assertion is on the branch `make_worktree` created, read back with `rev-parse` after
    /// the run: refs live in the common dir, so both branches are visible from either tree and
    /// the test cannot accidentally be asserting on "which repository has the ref". What
    /// separates a correct implementation from the one this design replaced is only **what
    /// commit** each branch points at.
    ///
    /// A supervisor-wide repository — of any spelling, including `launch.project_root` — makes
    /// both children branch from one commit and fails the inequality below. That failure is the
    /// bug this whole change exists to prevent, and it is silent in production: a real branch,
    /// off real commits, with a real worktree, and nothing anywhere reporting a problem.
    #[test]
    fn two_roots_in_different_worktrees_of_one_repository_branch_their_children_from_their_own_heads()
     {
        if !marion_testsupport::harness_available("claude") {
            return;
        }
        let dir = scratch("owns-two-worktrees");
        let main = fixture_repo(&dir);
        // Two linked worktrees, each carrying a commit the other does not have, so "branched
        // from the wrong tree" is visible as an oid and not merely as a path.
        let (side_a, head_a) = linked_worktree(&main, "a");
        let (side_b, head_b) = linked_worktree(&main, "b");
        assert_ne!(head_a, head_b, "the two trees must really differ");

        // **One supervisor, keyed the way production keys it.** Not `ProjectDir::new(&state,
        // &side_a)`: that is the mistake §2's rule exists to prevent, and it would give this
        // test two projects and no shared supervisor to prove anything about.
        let state = dir.join("state");
        let project = ProjectDir::new(&state, &crate::socket::project_root(&side_a));
        assert_eq!(
            project.path(),
            ProjectDir::new(&state, &crate::socket::project_root(&side_b)).path(),
            "§2 keys on the git common dir, so both worktrees are one project — one \
             supervisor, one journal. If this fails the rest of the test proves nothing."
        );
        std::fs::create_dir_all(project.path()).unwrap();

        let live = Arc::new(crate::registry::LiveRegistry::follow(
            Registry::boot_path(&project.journal()).unwrap(),
        ));
        for (seq, kind) in [
            intent("root-a", None, "claude", 0),
            intent("root-b", None, "claude", 0),
        ]
        .into_iter()
        .enumerate()
        {
            append(
                &project.journal(),
                &line(seq as u64, 1_000 + seq as u64, kind),
            );
        }
        live.refresh();
        let handle = RegistryHandle::owning(
            live,
            crate::run::Env {
                os_sandbox: true,
                project_dir: project.clone(),
                state: state.clone(),
                project_root: crate::socket::project_root(&main),
                bridge: std::path::PathBuf::from("/bin/marion-supervisor"),
                base_url: Some("http://127.0.0.1:8099/v1".into()),
                auth: marion_harness::Auth::Canned,
            },
            project.supervisor_sock(),
        );
        let fx = Owning {
            handle,
            project,
            // Never read by this test: each root states its own tree below. Present because the
            // fixture type is shared.
            repo: main.clone(),
            _dir: dir,
        };

        let child_of = |root: &str, tree: &Path| -> AgentId {
            let token = fx.handle.claim(
                &id(root),
                Some(marion_core::contract::TaskId(format!("t-{root}"))),
                tree.to_path_buf(),
            );
            let child = spawn(
                &fx,
                params(
                    Some(SpawnCaller {
                        agent_id: id(root),
                        node_token: token,
                    }),
                    5,
                ),
            )
            .unwrap_or_else(|e| panic!("{root}'s child must launch: {e:?}"))
            .agent_id;
            settle(&fx, &child);
            child
        };
        let child_a = child_of("root-a", &side_a);
        let child_b = child_of("root-b", &side_b);

        let base_of = |child: &AgentId| -> String {
            let branch = crate::spawn::checked_out_branch(crate::spawn::Tree::Operator(
                &fx.project.agent(child).worktree(),
            ))
            .expect("the child's worktree is on the branch marion cut for it");
            // Read out of the **main** repository: refs are shared across every worktree of one
            // repository, so this cannot be reading "the tree it was made in".
            let out = std::process::Command::new("git")
                .current_dir(&main)
                .args(["rev-parse", &branch])
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "`make_worktree` must have created {branch}: {out:?}"
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        assert_eq!(
            base_of(&child_a),
            head_a,
            "a child of the root in worktree `a` branches from **worktree a's** HEAD"
        );
        assert_eq!(
            base_of(&child_b),
            head_b,
            "and a child of the root in worktree `b` branches from worktree b's"
        );
        assert_ne!(
            base_of(&child_a),
            base_of(&child_b),
            "one supervisor, two trees, two bases. A supervisor-wide repository makes these \
             equal — which is a child silently branched off a tree its root is not in."
        );
    }

    /// A linked worktree of `main` carrying one commit of its own, and its HEAD oid.
    fn linked_worktree(main: &Path, tag: &str) -> (PathBuf, String) {
        let path = main.parent().expect("the repo has a parent").join(tag);
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(
            main,
            &["worktree", "add", "-q", "-b", tag, &path.to_string_lossy()],
        );
        std::fs::write(path.join(format!("{tag}.txt")), format!("{tag}\n")).unwrap();
        git(&path, &["add", "-A"]);
        git(
            &path,
            &[
                "-c",
                "user.email=marion@example.invalid",
                "-c",
                "user.name=marion",
                "commit",
                "-qm",
                tag,
            ],
        );
        let head = git(&path, &["rev-parse", "HEAD"]);
        (path, head)
    }

    /// A supervisor with no spawn environment refuses in its **own voice**, naming the
    /// constructor rather than failing further in on a directory. See
    /// `RegistryHandle::spawn_env`.
    ///
    /// `None` no longer means "production": stage 3 builds `owning`. It means this handle was
    /// built by `new`, which is a describing fixture, and the sentence says so.
    #[test]
    fn a_supervisor_that_cannot_spawn_refuses_by_naming_the_build_not_a_missing_directory() {
        let fx = fx("owns-no-env");
        let out = crate::serve::sink(ConnId(4));
        fx.handle.hello_as_operator(ConnId(4));
        let e = fx
            .handle
            .call(
                ConnId(4),
                // Well-formed, so the pairing check ahead of it passes and this really is the
                // refusal being asserted.
                &Call::AgentSpawn(root_params(Path::new("/r"), 1)),
                &out,
            )
            .expect_err("a handle built by `new` owns nothing");
        assert_eq!(e.kind(), Some(FailureKind::Unimplemented));
        assert!(
            e.message.contains("RegistryHandle::new"),
            "the refusal names the constructor, which is the whole of what is missing: {}",
            e.message
        );
    }

    // ------------------------------------------------------------------------------------
    // The token itself.
    // ------------------------------------------------------------------------------------

    /// Two nodes never share a capability, and a token is long enough that guessing is not a
    /// strategy. 32 bytes of `/dev/urandom` as hex is 64 characters.
    #[test]
    fn every_node_gets_its_own_unguessable_token() {
        let fx = owning("owns-tokens", vec![]);
        let a = fx.handle.claim(
            &id("a"),
            Some(marion_core::contract::TaskId("t".into())),
            fx.repo.clone(),
        );
        let b = fx.handle.claim(
            &id("b"),
            Some(marion_core::contract::TaskId("t".into())),
            fx.repo.clone(),
        );
        assert_ne!(
            a, b,
            "a per-node token that is not per-node is a fleet token"
        );
        assert_eq!(a.expose().len(), 64, "32 bytes as hex");
        assert!(a.expose().chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ------------------------------------------------------------------------------------
    // §5.7's exit predicate, and §7.3.1's departure.
    // ------------------------------------------------------------------------------------

    // ------------------------------------------------------------------------------------
    // The two that really launch a process.
    // ------------------------------------------------------------------------------------

    /// How long this file will wait for a node's thread to finish before calling it a leak.
    /// Never a verdict: every assertion below is over an identity, a pid or a count.
    const SETTLE: std::time::Duration = std::time::Duration::from_secs(90);

    /// Wait for **this node's** thread to reach an outcome, so the scratch directory is not
    /// removed out from under it. Asserts rather than returns: a node that never settles is
    /// exactly the leak these tests exist to catch.
    ///
    /// Per node and not `running_nodes() == 0`, because the *caller* in these fixtures is a
    /// node this supervisor also owns — `claim`ed to give it a token — and nothing ever
    /// finishes it. Waiting on the whole table would wait for a node that is a fixture.
    fn settle(fx: &Owning, agent_id: &AgentId) {
        let deadline = std::time::Instant::now() + SETTLE;
        while fx.handle.owned_running(agent_id) == Some(true) {
            assert!(
                std::time::Instant::now() < deadline,
                "a node's thread never produced an outcome within {}s",
                SETTLE.as_secs()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Spawn a real child and hand back the node's id, once the response has arrived.
    fn spawn_a_real_child(fx: &Owning, secs: u64) -> AgentId {
        let token = fx.handle.claim(
            &id("root"),
            Some(marion_core::contract::TaskId("t".into())),
            fx.repo.clone(),
        );
        spawn(
            fx,
            params(
                Some(SpawnCaller {
                    agent_id: id("root"),
                    node_token: token,
                }),
                secs,
            ),
        )
        .expect("the spawn is admitted and a process starts")
        .agent_id
    }

    /// An `Owning` fixture whose records are on disk **before** the registry boots — the
    /// inverse of [`owning`]'s order — so a node still `Live` at boot is one this supervisor
    /// never decided the fate of and the boot restart pass marks it `Orphaned` (§7.2). This is
    /// how a resume gets an orphan to relaunch.
    fn orphaning(tag: &str, records: Vec<RecordKind>) -> Owning {
        orphaning_with(tag, |_, _| records)
    }

    /// [`orphaning`] for records that have to **name the fixture's own directories** — a lost
    /// child's workspace is a path under the project, and the path is not knowable until the
    /// scratch repo exists. The closure is handed the repo and the project it was keyed to.
    fn orphaning_with(
        tag: &str,
        records: impl FnOnce(&std::path::Path, &ProjectDir) -> Vec<RecordKind>,
    ) -> Owning {
        let dir = scratch(tag);
        let repo = fixture_repo(&dir);
        let state = dir.join("state");
        let project = ProjectDir::new(&state, &crate::socket::project_root(&repo));
        std::fs::create_dir_all(project.path()).unwrap();
        let journal = project.journal();
        for (seq, kind) in records(&repo, &project).into_iter().enumerate() {
            append(&journal, &line(seq as u64, 1_000 + seq as u64, kind));
        }
        let live = Arc::new(crate::registry::LiveRegistry::follow(
            Registry::boot_path(&journal).unwrap(),
        ));
        let handle = RegistryHandle::owning(
            live,
            crate::run::Env {
                os_sandbox: true,
                project_dir: project.clone(),
                state: state.clone(),
                project_root: crate::socket::project_root(&repo),
                bridge: std::path::PathBuf::from("/bin/marion-supervisor"),
                base_url: Some("http://127.0.0.1:8099/v1".into()),
                auth: marion_harness::Auth::Canned,
            },
            project.supervisor_sock(),
        );
        Owning {
            handle,
            project,
            repo,
            _dir: dir,
        }
    }

    /// The records of a lost claude root: its intent, a `Spawned` with **no pid** (nothing to
    /// signal on relaunch), and the session its stream named. Booted through [`orphaning`],
    /// this replays `Orphaned` with `harness_session` set — exactly what resume requires.
    fn lost_root(
        session: &str,
        pid: Option<i32>,
        start_id: Option<marion_core::node::StartId>,
    ) -> Vec<RecordKind> {
        vec![
            intent("root", None, "claude", 0),
            RecordKind::Spawned(marion_core::journal::Spawned {
                agent_id: id("root"),
                harness_version: "test".into(),
                model: None,
                pid,
                start_id,
                provider: None,
                route: None,
                credential: None,
            }),
            RecordKind::SessionObserved(marion_core::journal::SessionObserved {
                agent_id: id("root"),
                harness: Harness::ClaudeCode,
                session_id: session.into(),
                pane: false,
                // A root derives its cwd from the project this supervisor serves, so its
                // launch names no workspace. The child fixture below is the one that does.
                workspace: None,
                profile: None,
            }),
        ]
    }

    /// The `TaskId` every lost-child fixture's contract is under.
    const CHILD_TASK: &str = "t-lost-child";

    /// The records of a **lost root and its lost child** — the shape a supervisor SIGKILL
    /// leaves behind. The child carries what a resume of it needs and a root's does not: the
    /// workspace it ran in, recorded on its `SessionObserved`.
    ///
    /// `workspace` is the caller's, so a test can name a tree that no longer exists (or none at
    /// all) without a second fixture. Neither node records a pid: `procid` is the root path's
    /// concern and is already measured there, and a fixture that recorded this process's pid
    /// would refuse for that reason instead of the one under test.
    fn lost_root_and_child(workspace: Option<marion_core::contract::Workspace>) -> Vec<RecordKind> {
        let spawned = |agent: &str| {
            RecordKind::Spawned(marion_core::journal::Spawned {
                agent_id: id(agent),
                harness_version: "test".into(),
                model: None,
                pid: None,
                start_id: None,
                provider: None,
                route: None,
                credential: None,
            })
        };
        let session = |agent: &str, ws: Option<marion_core::contract::Workspace>| {
            RecordKind::SessionObserved(marion_core::journal::SessionObserved {
                agent_id: id(agent),
                harness: Harness::ClaudeCode,
                session_id: format!("sess-{agent}"),
                pane: false,
                workspace: ws,
                profile: None,
            })
        };
        vec![
            intent("root", None, "claude", 0),
            spawned("root"),
            session("root", None),
            RecordKind::SpawnIntent(SpawnIntent {
                budget: None,
                review_of: None,
                agent_id: id("child"),
                parent_id: Some(id("root")),
                agent_type: "claude".into(),
                harness: Harness::ClaudeCode,
                depth: 1,
                task_id: Some(marion_core::contract::TaskId(CHILD_TASK.into())),
                timeout_secs: None,
                verification: vec![],
                race: None,
                workflow: None,
            }),
            spawned("child"),
            session("child", workspace),
        ]
    }

    /// The worktree a lost child ran in, **made on disk** so a resume of it finds the tree its
    /// session was created in still there.
    fn existing_child_worktree(project: &ProjectDir) -> marion_core::contract::Workspace {
        let path = project.agent(&id("child")).worktree();
        std::fs::create_dir_all(&path).unwrap();
        marion_core::contract::Workspace::Worktree {
            path,
            branch: format!("marion/{CHILD_TASK}"),
        }
    }

    fn resume(
        fx: &Owning,
        agent_id: AgentId,
        prompt: &str,
    ) -> Result<marion_core::proto::result::NodeResumeResult, RpcError> {
        let out = crate::serve::sink(ConnId(3));
        fx.handle.hello_as_operator(ConnId(3));
        match fx.handle.call(
            ConnId(3),
            &Call::NodeResume(marion_core::proto::params::NodeResumeParams {
                agent_id,
                prompt: prompt.into(),
            }),
            &out,
        )? {
            MethodResult::NodeResume(r) => Ok(r),
            other => panic!("wrong result: {}", other.method().as_str()),
        }
    }

    /// **A resume relaunches an orphan into its own id, through `agent/spawn`'s launch path.**
    ///
    /// The orphan's process is gone (no pid), so the preflight proceeds straight to the
    /// launcher; `node/resume` answers with the **same** agent id and the next
    /// `spawn_generation`, and a second `Spawned` for that id lands on the one journal — replay
    /// then folds it as generation two. Nothing about the launch is a new node.
    #[test]
    fn resume_relaunches_an_orphan_into_its_own_node_id_through_agent_spawns_path() {
        if !marion_testsupport::harness_available("claude") {
            return;
        }
        let fx = orphaning("resume-relaunch", lost_root("sess-relaunch", None, None));
        // The orphan is what a resume is for: fate marked, a session to hand back.
        fx.handle.hello_as_operator(ConnId(9));
        let before = fx
            .handle
            .call(
                ConnId(9),
                &Call::NodeGet(marion_core::proto::params::NodeGetParams::of(id("root"))),
                &crate::serve::sink(ConnId(9)),
            )
            .expect("the orphan is on the tree");
        let MethodResult::NodeGet(before) = before else {
            panic!("node/get")
        };
        assert_eq!(before.node.reap_state, ReapState::Orphaned);

        let r = resume(&fx, id("root"), "carry on from here").expect("the orphan relaunches");
        assert_eq!(r.agent_id, id("root"), "the node keeps its own id");
        assert_eq!(r.spawn_generation, 2, "the second lifetime of one node");
        settle(&fx, &id("root"));

        let journalled = std::fs::read_to_string(fx.project.journal()).unwrap();
        let spawns = journalled
            .lines()
            .filter(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v["kind"]["Spawned"]["agent_id"] == serde_json::json!("root")
            })
            .count();
        assert_eq!(
            spawns, 2,
            "the relaunch wrote a second `Spawned` for the same id:\n{journalled}"
        );
        assert_eq!(
            marion_core::registry::replay(journalled.as_bytes())
                .get(&id("root"))
                .unwrap()
                .spawn_generation,
            2,
            "replay folds the second spawn as generation two"
        );
    }

    /// **A resume relaunches a lost child into its own id, under its own parent.**
    ///
    /// The root path could rebuild a root's launch from the project the supervisor is keyed on.
    /// A child's could not, until its workspace was journaled: its cwd is a linked worktree
    /// marion made, and a relaunch anywhere else reaches the harness with a cwd the session was
    /// not created in. Now it is recorded, so the child goes back through the **same**
    /// `run::run_spawn` path a fresh child takes — its own `AgentId`, its recorded parent, its
    /// recorded depth, and `resume: Some(session)` — and lands in the tree it left.
    ///
    /// The contract the run writes is the oracle for *where*: §6.7 records the workspace, so a
    /// relaunch that had cut a second worktree would name a different path there.
    #[test]
    fn resume_relaunches_a_lost_child_into_its_own_node_id_under_its_parent() {
        if !marion_testsupport::harness_available("claude") {
            return;
        }
        let fx = orphaning_with("resume-child", |_, project| {
            lost_root_and_child(Some(existing_child_worktree(project)))
        });
        let expected = existing_child_worktree(&fx.project);

        let r = resume(&fx, id("child"), "carry on from here").expect("the child relaunches");
        assert_eq!(r.agent_id, id("child"), "the node keeps its own id");
        assert_eq!(r.spawn_generation, 2, "the second lifetime of one node");
        settle(&fx, &id("child"));

        let journalled = std::fs::read_to_string(fx.project.journal()).unwrap();
        let node = marion_core::registry::replay(journalled.as_bytes())
            .get(&id("child"))
            .cloned()
            .expect("the child still replays under its own id");
        assert_eq!(node.spawn_generation, 2, "replay folds the second spawn");
        assert_eq!(
            node.depth(),
            Some(1),
            "§7.5 makes the intent immutable, so the second life is at the same depth"
        );
        assert_eq!(
            node.intent.as_ref().and_then(|i| i.parent_id.clone()),
            Some(id("root")),
            "and under the same parent: the tree shows it where it was"
        );

        // **And it ran in the tree the journal recorded.** The fixture's worktree is a plain
        // directory rather than a real linked worktree, so `make_worktree` would have failed on
        // it and the resume would have returned an error instead of a node — a relaunch that
        // cut a second tree cannot reach this line. The directory is still the one the fixture
        // made, untouched by git. `select_workspace`'s own unit test asserts the value
        // directly; this asserts the launch took that path.
        assert!(
            expected.path().is_dir() && !expected.path().join(".git").exists(),
            "the recorded tree is still the one the first life used: {}",
            expected.path().display()
        );
    }

    /// **A resume refuses by name when the tree the session was created in is gone.** `marion
    /// run`'s cleanup and `worktree_reap` both remove a child's worktree; the session id
    /// outlives it on the journal, and handing it back from a directory the harness has never
    /// seen is how a "resume" silently becomes a fresh run under a resumed node's id.
    #[test]
    fn resume_refuses_a_child_whose_recorded_worktree_is_gone() {
        let fx = orphaning_with("resume-child-reaped", |_, project| {
            lost_root_and_child(Some(marion_core::contract::Workspace::Worktree {
                path: project.agent(&id("child")).worktree(),
                branch: format!("marion/{CHILD_TASK}"),
            }))
        });
        let before = journal_len(&fx);
        let e = resume(&fx, id("child"), "carry on").expect_err("a reaped tree blocks it");
        assert_eq!(e.kind(), Some(FailureKind::Refused), "{e:?}");
        assert!(
            e.message.contains("no longer exists"),
            "the refusal names the missing tree: {}",
            e.message
        );
        assert_eq!(journal_len(&fx), before, "nothing was launched");

        // And a child whose journal never named a workspace at all is refused for that, rather
        // than relaunched in whatever directory is at hand.
        let fx = orphaning_with("resume-child-unrecorded", |_, _| lost_root_and_child(None));
        let e = resume(&fx, id("child"), "carry on").expect_err("an unrecorded tree blocks it");
        assert_eq!(e.kind(), Some(FailureKind::Refused), "{e:?}");
        assert!(
            e.message.contains("does not record"),
            "the refusal says the journal is silent: {}",
            e.message
        );
    }

    /// **A resume refuses a child whose parent is still Live.** Principle 8: marion holds a
    /// running node's channel, and a Live parent's own `spawn` owns this child — its outcome is
    /// owed to a call that is still waiting for it. Relaunching from outside would put a second
    /// process under one contract, which is the thing resume exists not to do. A parent whose
    /// fate is decided holds nothing, and that child resumes.
    #[test]
    fn resume_refuses_a_child_whose_parent_is_still_live() {
        // `owning` writes its records **after** the boot pass, so nothing is marked `Orphaned`:
        // the root is a node this supervisor holds, and the child is resumable only because its
        // own `Exited` is on the journal.
        let mut records = lost_root_and_child(None);
        records.push(RecordKind::Exited(marion_core::journal::Exited {
            agent_id: id("child"),
            status: marion_core::contract::ResultStatus::Failed,
            exit: ProcessExit {
                code: Some(1),
                signal: None,
                description: "the child's first life ended".into(),
            },
        }));
        let fx = owning("resume-child-live-parent", records);
        let before = journal_len(&fx);
        let e = resume(&fx, id("child"), "carry on").expect_err("a live parent blocks it");
        assert_eq!(e.kind(), Some(FailureKind::Refused), "{e:?}");
        assert!(
            e.message.contains("still live"),
            "the refusal names the parent: {}",
            e.message
        );
        assert_eq!(journal_len(&fx), before, "nothing was launched");
    }

    /// **A resume refuses when the orphan's own process cannot be proven gone.** A recorded pid
    /// that is still alive with no recorded identity is `procid::CannotTell`: marion will not
    /// start a second process that might run against a transcript a first is still writing, so
    /// it refuses and launches nothing.
    #[test]
    fn resume_refuses_when_the_orphans_process_cannot_be_identified() {
        // This test's own pid is alive, and the orphan recorded no start identity — so a probe
        // of it reads a live process marion cannot prove is or is not the node's.
        let fx = orphaning(
            "resume-cannot-tell",
            lost_root("sess-cannot", Some(std::process::id() as i32), None),
        );
        let before = journal_len(&fx);
        let e = resume(&fx, id("root"), "carry on")
            .expect_err("an unprovable process blocks the resume");
        assert_eq!(e.kind(), Some(FailureKind::Conflict), "{e:?}");
        assert!(
            e.message.contains("cannot prove"),
            "the refusal names why: {}",
            e.message
        );
        assert_eq!(
            journal_len(&fx),
            before,
            "nothing was signalled or launched, so nothing was journaled"
        );
    }

    /// **The response's `state` is a claim the journal already backs.**
    ///
    /// This is the whole reason `agent/spawn` returns at the `on_started` hook rather than
    /// before it or after the run. Before it, the answer would be a promise: a client told
    /// `Spawning` would have nothing on disk to read, and a supervisor that then died would
    /// leave a `SpawnIntent` meaning either "nothing was started" or "something is running and
    /// marion cannot name it" — §11 item 30's two indistinguishable shapes. After the run, the
    /// call would be a minutes-long synchronous JSON-RPC request, which is what
    /// `background.rs` records as taking a whole bridge down when it hangs.
    ///
    /// So the assertion is not that the pid is plausible but that **the record is already
    /// there when the caller has the answer**, with the same pid the table holds. The journal
    /// is read from the file rather than from the follower, because the follower is a poll and
    /// would let a record that had not been written yet appear a few milliseconds later.
    #[test]
    fn the_spawn_response_names_a_node_whose_spawned_record_is_already_on_disk() {
        if !marion_testsupport::harness_available("claude") {
            return;
        }
        let fx = owning("owns-launch", vec![intent("root", None, "claude", 0)]);
        let agent_id = spawn_a_real_child(&fx, 5);

        let journalled = std::fs::read_to_string(fx.project.journal()).unwrap();
        let spawned: Vec<serde_json::Value> = journalled
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|v| v["kind"]["Spawned"]["agent_id"] == serde_json::json!(agent_id.0))
            .collect();
        assert_eq!(
            spawned.len(),
            1,
            "the caller has its answer, so the node's `Spawned` must already be on disk:\n{journalled}"
        );
        let journal_pid = spawned[0]["kind"]["Spawned"]["pid"]
            .as_i64()
            .map(|p| p as i32);
        assert!(
            journal_pid.is_some_and(|p| p > 0),
            "step 1 makes this a real signal target, not a `None`: {:?}",
            spawned[0]
        );
        assert_eq!(
            fx.handle.owned_pid(&agent_id),
            journal_pid,
            "the table and the journal must name one pid, or a `session/quit` KillTree and \
             this supervisor's own handle would signal different processes"
        );
        assert!(
            fx.handle.owned_pgid(&agent_id).is_some_and(|g| g > 0),
            "§6.7 kills a per-node process group, so the group has to be recorded — and read \
             with getpgid(2), never assumed equal to the pid"
        );
        assert!(
            fx.handle.owned_started_at(&agent_id).is_some(),
            "the instant the process came into existence"
        );
        assert!(
            fx.handle.owned_task_id(&agent_id).is_some(),
            "the contract this node runs under"
        );
        assert_eq!(fx.handle.owned_nodes(), 2, "the caller and its child");
        settle(&fx, &agent_id);
    }

    /// **The bound a caller asked for is the bound the tree reports.**
    ///
    /// `marion tree`'s detail pane prints `NodeSummary.timeout`, and until the intent recorded
    /// one there was nothing in the journal to print: the projection re-resolved §3.1's bound
    /// from the *agent type*, so every node on the screen read 900 s however short a clock the
    /// operator or the parent had actually put it under. A pane that reports a bound no node is
    /// running under is worse than one that reports none — it is the wrong number in the one
    /// place an operator looks to decide whether a run has time left.
    ///
    /// Over a **real** child, through `agent/spawn` and back out of `tree/subscribe`, because
    /// the two halves this pins are a write and a read on opposite sides of the journal.
    #[test]
    fn a_childs_requested_bound_is_what_the_tree_reports() {
        if !marion_testsupport::harness_available("claude") {
            return;
        }
        let fx = owning("owns-child-bound", vec![intent("root", None, "claude", 0)]);
        let agent_id = spawn_a_real_child(&fx, 120);

        let out = crate::serve::sink(ConnId(4));
        fx.handle.hello_as_operator(ConnId(4));
        let MethodResult::TreeSubscribe(snap) = fx
            .handle
            .call(
                ConnId(4),
                &Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
                &out,
            )
            .expect("the tree is readable")
        else {
            panic!("wrong result type")
        };
        let child = snap
            .nodes
            .iter()
            .find(|n| n.agent_id == agent_id)
            .expect("the spawned child is in the tree");
        assert_eq!(
            child.timeout,
            marion_core::encoding::Duration::from_secs(120),
            "`spawn`'s `timeout_secs` is the clock this node runs under, so it is the clock \
             the tree must show — not its agent type's default"
        );
        assert_ne!(
            child.timeout,
            agent_type::builtin("claude").unwrap().timeout,
            "and the assertion above is not passing by coincidence with §3.1's default"
        );

        if let Some(pid) = fx.handle.owned_pid(&agent_id) {
            crate::kill::kill_process_tree_and_wait(pid);
        }
        settle(&fx, &agent_id);
    }

    /// **§7.3.1: `gone` touches nothing — and after this step that covers a *bridge*.**
    ///
    /// The comment on `Handle::gone` has always said so; this asserts it, because a comment is
    /// not a test and this is the step that makes the invariant load-bearing. While every node
    /// was owned by the bridge process the harness started, a SIGKILL of that bridge — which
    /// s16 measured as what a real Claude Code harness sends, uncatchable, ~450 ms after its
    /// SIGTERM — orphaned a live process at pid 1 with an unresolved `SpawnIntent` and no pid
    /// to recover it by. Once the supervisor owns the node, the same kill closes a socket.
    ///
    /// **Liveness by S15's three-valued `ps`, never `kill(pid, 0)`**, which reports a zombie as
    /// alive and would let a node that died a millisecond before the departure satisfy this.
    #[test]
    fn a_departing_client_does_not_touch_a_node_the_supervisor_owns() {
        if !marion_testsupport::harness_available("claude") {
            return;
        }
        let fx = owning("owns-departure", vec![intent("root", None, "claude", 0)]);
        let agent_id = spawn_a_real_child(&fx, 30);

        let before = (
            fx.handle.owned_pid(&agent_id),
            fx.handle.owned_pgid(&agent_id),
            fx.handle.owned_task_id(&agent_id),
            fx.handle.owned_started_at(&agent_id),
            fx.handle.owned_nodes(),
            fx.handle.running_nodes(),
        );
        let pid = before.0.expect("a process exists");
        assert_eq!(
            marion_testsupport::liveness(pid),
            marion_testsupport::Liveness::Alive,
            "the premise of this test is a live node; without it the assertions below are \
             vacuous"
        );

        fx.handle
            .gone(ConnId(3), &ClientGone::SocketClosed, &Departure::Eof);

        assert_eq!(
            marion_testsupport::liveness(pid),
            marion_testsupport::Liveness::Alive,
            "a client's departure must not reach the node's process (§7.3.1)"
        );
        assert_eq!(
            (
                fx.handle.owned_pid(&agent_id),
                fx.handle.owned_pgid(&agent_id),
                fx.handle.owned_task_id(&agent_id),
                fx.handle.owned_started_at(&agent_id),
                fx.handle.owned_nodes(),
                fx.handle.running_nodes(),
            ),
            before,
            "…and must not reach the supervisor's record of it either"
        );

        // The node is still held, so §5.7 still refuses to exit — the other half of the same
        // invariant, and what stops a departure from becoming a fleet-wide shutdown.
        assert!(
            !fx.handle.idle_exit_eligible(),
            "a supervisor whose last client left still owns a running node"
        );

        // Ended deliberately rather than left to the 30 s bound, so the file leaves no
        // survivor and no scratch directory behind. This is cleanup, not an assertion.
        crate::kill::kill_process_tree_and_wait(pid);
        settle(&fx, &agent_id);
    }

    /// **The node table as §5.7's second guard.**
    ///
    /// The journal here says nothing at all — no node, no intent — so `resident_reason` is `None`
    /// and a journal-only predicate would let this supervisor exit. A node whose thread is running
    /// and whose journal write failed is exactly that shape, and exiting through it leaves the
    /// untracked live process §9's M2 criteria forbid.
    #[test]
    fn a_node_this_supervisor_still_runs_holds_it_even_when_the_journal_says_nothing() {
        let fx = owning("owns-exit-guard", vec![]);
        assert!(
            fx.handle.idle_exit_eligible(),
            "an empty supervisor with no clients may exit"
        );
        fx.handle.claim(
            &id("ghost"),
            Some(marion_core::contract::TaskId("t".into())),
            fx.repo.clone(),
        );
        assert!(
            !fx.handle.idle_exit_eligible(),
            "a node this process still owns and has no outcome for must hold the supervisor, \
             however quiet the journal is"
        );
        fx.handle.mark_finished(
            &id("ghost"),
            NodeOutcome::Child(Box::new(Err(crate::spawn::SpawnError::UnknownAgentType(
                "x".into(),
            )))),
        );
        assert!(
            fx.handle.idle_exit_eligible(),
            "…and must stop holding it once its thread has produced an outcome"
        );
    }
}
