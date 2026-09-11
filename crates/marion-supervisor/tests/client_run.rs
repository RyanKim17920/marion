//! **`marion run` is a client, and M2's acceptance criteria 1 and 2** (§9, §11 item 28 step 6).
//!
//! > *"A TUI crash cannot kill running agents: agents keep running, and a new client shows the full
//! > tree."*
//!
//! Criterion 2 is measured here too, and in the same bed, because it is the same kill: criterion 1
//! is that the tree *survives* the client, criterion 2 is that the tree a replacement client is
//! shown **is the journal** — same nodes, same parent edges, same terminal states, same contracts,
//! compared as of that client's own read. See
//! [`a_new_clients_tree_is_the_journal_the_supervisor_read_including_the_window_no_client_saw`].
//!
//! Until step 6 this criterion was unmeetable and `MILESTONES.md` said so: `marion run` held the
//! root's `Child`, its pipe and its whole turn, so killing it killed the root. Now the supervisor
//! owns the root and `marion run` is a socket client that spawns, attaches and renders — so the
//! criterion is a `kill -9` away from being a measurement, and this file is that measurement.
//!
//! # What is asserted, and why each clause is here
//!
//! The kill is of the **client**, and the assertions afterwards are §9's criterion-4 shape applied
//! to it, because "a new client shows the full tree" is worth nothing if the tree it shows is a
//! corpse:
//!
//! * **(i) the same supervisor** — by pid *and* the kernel's start identity for whatever wears it,
//!   so a reissued pid cannot satisfy it. `restart.rs` is emphatic that a bare pid is not an
//!   identity, and the comparison is `procid::resolve`'s rather than one restated here.
//! * **(ii) events emitted after the new client attached** — §6.1 step 8's *MUST NOT substitute a
//!   sleep for an observation* binds a test as hard as it binds a launcher, so the node's next turn
//!   is **held** at the provider until the attach has happened, and the ordering is proved from the
//!   provider's own request log rather than from this file's say-so.
//! * **(iii) contiguity across the detached window** — one file's own ordinals, read by one cursor:
//!   a gap means an event was dropped, a repeat means the replay and subscribe legs overlapped.
//! * **liveness, and then growth.** A pid in the process table can be a zombie, so liveness is S15's
//!   three-valued `procid` reading (`marion_testsupport::liveness`) and never `kill(pid, 0)`. And a
//!   live process that has stopped working would satisfy even that, so the load-bearing assertion is
//!   that the root's `events.jsonl` **grew after the kill**.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test client_run
//! ```
//!
//! The five tests that start a `marion run` need real `claude` and `codex` on `PATH`, and they say
//! so through `common::script::require_claude_and_codex` — so on every machine a missing or drifted
//! binary still panics naming the pinned version, and only a runner that has set
//! `MARION_CI_NO_HARNESSES=1` skips, by name, on the uncaptured stderr. It was the absence of that
//! line that put this suite on CI's harness-free list: the list is grepped for `on_path`, this file
//! mentioned neither it nor `PINNED_HARNESSES`, and so all five failed on both runners naming a
//! journal invariant rather than the missing `claude`. The two tests that launch nothing — the
//! refused run and the reissued pid — are ungated and still run everywhere.
//!
//! Every model call is served by the CannedServer: no paid tokens.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use marion_core::contract::AgentId;
use marion_core::event::{Lifecycle, Payload};
use marion_core::node::StartId;
use marion_core::proto::notify::Event as Note;
use marion_provider::script::classify_root;
use marion_provider::{
    CannedServer, Config, RequestKind, RootStep, Script, TurnGate, classify_anthropic,
};
use marion_supervisor::procid::{self, Resolution};
use marion_supervisor::socket::{SocketPaths, read_identity};
use marion_testsupport::{Liveness, fixture_repo, liveness, scratch, sweep};

mod common;
use common::client::{Client, paths_for};
use common::run::start_run;
use common::script::{Delegation, claude_delegates_to_codex};

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// How long anything in this file may take before it is a failure. Never a verdict: every assertion
/// below is over an identity, an ordinal, a count, a payload or a liveness reading — never over
/// elapsed time — and no failure here may be repaired by widening this.
const BOUND: Duration = Duration::from_secs(180);

const ROOT_MARKER: &str = "MARION-CLIENT-RUN-ROOT-TURN-b41c";
/// Appears in the **root's** closing turn and nowhere else, so an event carrying it can only have
/// come from the provider request the test released after the new client attached.
const ROOT_FINAL_MARKER: &str = "MARION-CLIENT-RUN-AFTER-RELEASE-9d3e";
const NARRATIVE: &str = "Wrote the marker under src/ and reported back.";
const CHILD_FILE: &str = "src/client-run-marker.txt";
const CHILD_TIMEOUT_SECS: u64 = 120;
const ROOT_BLOCKED_SECS: &str = "5";
/// The child's wire, and the one whose turn is held. Counted per wire because arrival order across
/// the port is a race by construction — see `marion_provider::gate`.
const CHILD_WIRE: &str = "responses";
/// The root's wire. A claude root speaks Anthropic messages.
const ROOT_WIRE: &str = "anthropic";

fn script() -> Script {
    claude_delegates_to_codex(Delegation {
        root_marker: ROOT_MARKER,
        root_final_text: ROOT_FINAL_MARKER,
        child_narrative: NARRATIVE,
        child_file: CHILD_FILE,
        child_file_line: "marion client-run marker",
        child_final_narrative: "the child is done",
        child_timeout_secs: CHILD_TIMEOUT_SECS,
    })
}

/// `(agent_seq, payload)` for the `node/event` notifications about one node, in arrival order.
fn stream(notes: &[Note], about: &AgentId) -> Vec<(u64, serde_json::Value)> {
    notes
        .iter()
        .filter_map(|n| match n {
            Note::NodeEvent {
                agent_id,
                agent_seq,
                payload,
                ..
            } if agent_id == about => Some((*agent_seq, payload.clone())),
            _ => None,
        })
        .collect()
}

/// §9's *"contiguous across the detached window"*, as an assertion rather than a hope.
///
/// Legitimate here in a way §4.2 forbids generally — that section refuses inferred ordering across
/// *sources* — because these are one file's own ordinals, assigned by that file's single writer and
/// read by one cursor.
fn assert_contiguous_from(seqs: &[u64], first: u64, what: &str) {
    assert!(!seqs.is_empty(), "{what}: nothing arrived");
    let want: Vec<u64> = (first..first + seqs.len() as u64).collect();
    assert_eq!(seqs, want.as_slice(), "{what}: not contiguous, or repeated");
}

// ------------------------------------------------------------------------------------------
// Facts about the supervisor, the node and the provider, read from outside.
// ------------------------------------------------------------------------------------------

/// A supervisor's identity as something a *recycled pid* cannot forge: the pid beside the kernel's
/// **start identity** for whatever wears it.
///
/// This used to shell out to `ps -o lstart=`. Same idea, three defects the direct read does not
/// have, and `procid`'s own module doc measured all three on this platform:
///
/// * **cost** — `sysctl(KERN_PROC_PID)` is 15.2 µs against `ps`'s 4.35 ms *and a fork*, about 285×.
///   A fork per reading is what confined this check to a snapshot at either end of a test; at 15.2
///   µs it can be polled, which is what [`while_the_supervisor_holds`] does and is a different and
///   stronger kind of assertion;
/// * **resolution** — `lstart` is a one-second clock, so two processes born inside the same second
///   are indistinguishable to it. `p_starttime` is a `timeval`, so they are not. The property this
///   function exists for is *strictly stronger* after the change, not merely cheaper;
/// * **the reading itself** — `procid` distinguishes *"no process wears this pid"* from *"marion
///   could not ask"*, where an empty `ps` stdout conflates them.
///
/// It also retires the last `ps -o lstart=` fork in the workspace, which `procid`'s module doc
/// already argued against on the boundary `Cargo.toml` draws around shelling out.
fn supervisor_identity(paths: &SocketPaths) -> (i32, StartId) {
    let pid = read_identity(paths)
        .expect("a serving supervisor publishes its identity")
        .pid;
    match procid::read(pid) {
        procid::Read::Id(start) => (pid, start),
        other => panic!("the kernel cannot identify the serving supervisor's pid {pid}: {other:?}"),
    }
}

/// Whether the process serving this project **is the one [`supervisor_identity`] named earlier**.
///
/// The comparison is `procid::resolve`'s and deliberately not a tuple `==` written out here. That
/// function is the one place in the workspace where *"the pid matches but the start identity does
/// not"* is decided, its mismatch arm answers `Gone` rather than a doubt, and
/// `procid::tests::the_resolution_table_is_exactly_the_four_rows_and_a_mismatch_is_evidence` kills
/// any build that softens it. Routing the same-process assertions through it means the reissued-pid
/// property they rest on cannot be weakened without a named test dying — which is what the `ps`
/// tuple bought by hand and what a hand-written tuple here would silently give back.
fn still_the_same_supervisor(paths: &SocketPaths, before: &(i32, StartId)) -> Resolution {
    // **No serving supervisor is `Gone`, not a panic.** `read_identity` answers `None` only while
    // the lock behind it is free, which is proof that nothing is serving this project — so the
    // supervisor that was fingerprinted is not there, which is the answer this function exists to
    // give. Panicking here instead would report the most interesting failure these assertions have
    // — *the supervisor left* — as a missing file, and the caller's sentence would never print.
    let Some(id) = read_identity(paths) else {
        return Resolution::Gone;
    };
    if id.pid != before.0 {
        // A different pid is a different process with no kernel reading required — and saying so
        // here keeps `resolve` answering the only question it is about, which is same-pid identity.
        return Resolution::Gone;
    }
    procid::resolve(Some(&before.1), procid::read(id.pid))
}

/// How many times the provider has been asked for one step of the **root's own** script — its
/// account of what it was asked, not the test's account of what it expected.
///
/// Filtered four ways, and every filter is load-bearing rather than tidy:
///
/// * **the wire**, because the child speaks a different one;
/// * **[`RequestKind::ScriptedTurn`]**, because Claude Code issues a *concurrent* session-title
///   request on the same wire that belongs to no node at all. A bare per-wire count therefore
///   measures a race: it is 1 or 2 at the same point in the same run depending on whether that
///   request has landed, and an assertion over it is not an assertion over anything the node did.
/// * **the marker**, because it is the same discriminator `Script::respond` itself uses to decide
///   whose turn a request is ([`RootScript::marker`]) — so this reads the provider's log by the
///   provider's own rule rather than by a second one invented here;
/// * **the step**, because *which* turn was asked for is the whole question. `RootStep::Finish` is
///   the root's closing turn, the one that produces [`ROOT_FINAL_MARKER`], and *"it had not been
///   asked for at attach time"* is the checkable fact that stands in for a sleep.
fn root_turns_asked(server: &CannedServer, step: RootStep) -> usize {
    server
        .requests()
        .expect("the request log is readable")
        .iter()
        .filter(|r| r.get("wire").and_then(serde_json::Value::as_str) == Some(ROOT_WIRE))
        .filter_map(|r| r.get("body"))
        .filter(|b| classify_anthropic(b) == RequestKind::ScriptedTurn)
        .filter(|b| b.to_string().contains(ROOT_MARKER))
        .filter(|b| classify_root(b) == step)
        .count()
}

fn until(cond: impl FnMut() -> bool) -> bool {
    until_within(BOUND, cond)
}

/// [`until`] with its own budget, for the one wait whose *expiry is itself a defect report*.
///
/// The registry polls every 10 ms (`detach::REGISTRY_POLL`), so a supervisor catching up with a
/// journal that has stopped growing is a matter of milliseconds. A wait for that which runs to
/// [`BOUND`] would turn a follower that has permanently lost a record — the seam defect criterion 2
/// is about — into a three-minute timeout instead of a named failure. So the caller gives it a
/// short budget and then *asserts about the numbers*, which is what says which record went missing.
fn until_within(budget: Duration, cond: impl FnMut() -> bool) -> bool {
    marion_testsupport::until_within(budget, Duration::from_millis(5), cond)
}

/// [`until_within`] with **§9 criterion 4's clause (i) as a standing invariant of the wait**, not
/// as a snapshot taken once the wait is over.
///
/// The natural shape is to wait out the detached window and *then* ask whether the supervisor is
/// still there, and it is the wrong one: a supervisor that left the moment its last client did is
/// then discovered by the *expiry* of a wait for something it was supposed to be driving. That is a
/// defect reported as a timeout, which is what §9's budgets exist to avoid — and clause (i), the
/// thing actually broken, never fails at all. Measured: as a trailing snapshot that build failed in
/// 31 s at the wrong sentence; as an invariant it fails in 1 s at the right one.
///
/// So the identity is re-read on every 5 ms tick and a change is an immediate panic naming clause
/// (i). **This is affordable only because the reading no longer forks**: at `ps`'s measured 4.35 ms
/// it would have been most of a tick and a fork per tick, which is why the check used to be a
/// snapshot. At `sysctl`'s 15.2 µs it is 0.3% of a tick, so the strongest available form of the
/// clause — *held throughout* rather than *true at one instant* — is also the cheap one.
fn while_the_supervisor_holds(
    budget: Duration,
    paths: &SocketPaths,
    before: &(i32, StartId),
    mut cond: impl FnMut() -> bool,
) -> bool {
    until_within(budget, || {
        assert_eq!(
            still_the_same_supervisor(paths, before),
            Resolution::AliveAndOurs,
            "**§9 criterion 4 (i)**: the supervisor stopped being the process it was, *during* the \
             detached window. Its last client left voluntarily and §7.3.2 waives §5.7's grace for a \
             client that announced itself — so the only thing keeping this process alive was §5.7's \
             other clause, a node that has not finished, and a build that reads a clean quit as its \
             own cue to go walks straight through it. Same pid *and* same kernel start identity, so \
             a reissued pid cannot paper over it"
        );
        cond()
    })
}

fn project(state: &Path, repo: &Path) -> marion_core::paths::ProjectDir {
    marion_core::paths::ProjectDir::new(state, &marion_supervisor::socket::project_root(repo))
}

/// **The journal, read from a second process** — which is the whole point of every assertion that
/// uses it. Nothing here shares memory with the supervisor.
fn journal_nodes(state: &Path, repo: &Path) -> Vec<marion_core::registry::ReplayedNode> {
    let bytes = std::fs::read(project(state, repo).journal()).unwrap_or_default();
    let mut replay = marion_core::registry::Replay::default();
    replay.extend(&bytes);
    replay.nodes().to_vec()
}

fn the_root(nodes: &[marion_core::proto::NodeSummary]) -> AgentId {
    let mut roots: Vec<&marion_core::proto::NodeSummary> =
        nodes.iter().filter(|n| n.depth == 0).collect();
    assert_eq!(roots.len(), 1, "this run has exactly one root: {nodes:?}");
    roots.pop().unwrap().agent_id.clone()
}

fn events_len(state: &Path, repo: &Path, node: &AgentId) -> u64 {
    std::fs::metadata(project(state, repo).agent(node).events())
        .map(|m| m.len())
        .unwrap_or(0)
}

// ------------------------------------------------------------------------------------------
// T5 — M2 acceptance criterion 1
// ------------------------------------------------------------------------------------------

/// **M2 criterion 1: SIGKILL the client, and the agents keep running.**
#[test]
fn a_killed_client_leaves_its_agents_running_and_a_new_client_sees_the_whole_tree() {
    if !common::script::require_claude_and_codex() {
        return;
    }
    let dir = scratch("client-run-kill");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    // Hold the child's **second** turn, which parks the whole tree: the child is waiting on the
    // provider, and the root is waiting on the child's `spawn` to return. Everything below happens
    // while both nodes are alive and neither can finish without this test's permission.
    let gate = TurnGate::holding_from(CHILD_WIRE, 2);
    let server = CannedServer::start_gated(
        Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog: dir.join("provider-requests.jsonl"),
            script: script(),
        },
        Some(Arc::clone(&gate)),
    )
    .expect("the canned provider binds");

    let mut run = start_run(
        &dir,
        &repo,
        &state,
        &server.base_url(),
        &gate,
        ROOT_MARKER,
        ROOT_BLOCKED_SECS,
    );
    let client_pid = run.pid();
    let paths = paths_for(&state, &repo);

    assert!(
        until(|| gate.parked() == 1),
        "the child's second turn is held, so the tree is parked with its question asked"
    );

    // ---- (i) the supervisor, before ---------------------------------------------------------
    let before = supervisor_identity(&paths);
    assert_ne!(
        before.0, client_pid,
        "the supervisor is a **different process** from the client — without that the rest of this \
         test is measuring nothing"
    );

    // The root's own pid, read out of the journal by a second process. This is the number the whole
    // criterion turns on: it is what the supervisor journaled at `command.spawn()`, and it must
    // outlive the client.
    let root_pid = until_root_pid(&state, &repo);
    assert_eq!(
        liveness(root_pid),
        Liveness::Alive,
        "the premise: the root is running before anything is killed"
    );

    // ---- the client is killed, uncatchably ---------------------------------------------------
    run.kill_client();
    assert_eq!(
        liveness(client_pid),
        Liveness::Gone,
        "the client is really gone, so nothing below can be crediting it"
    );
    let at_kill = events_len(&state, &repo, &root_from_journal(&state, &repo));
    let root = root_from_journal(&state, &repo);

    // ---- the agents kept running -------------------------------------------------------------
    assert_eq!(
        liveness(root_pid),
        Liveness::Alive,
        "**M2 criterion 1**: a client's crash must not reach the node's process. Liveness is S15's \
         three-valued reading and never `kill(pid, 0)`, which reports a zombie as alive"
    );
    assert_eq!(
        still_the_same_supervisor(&paths, &before),
        Resolution::AliveAndOurs,
        "the supervisor outlived the client that started it — by pid *and* start identity, so a \
         reissued pid cannot satisfy it"
    );

    // ---- a new client shows the full tree ----------------------------------------------------
    let mut b = Client::dial(&paths);
    let nodes = b.tree();
    assert_eq!(
        nodes.len(),
        2,
        "*the full tree*: the root the dead client asked for, and the child it spawned — {nodes:?}"
    );
    assert_eq!(
        the_root(&nodes),
        root,
        "and the root is the one still running"
    );

    let closing_turns_at_attach = root_turns_asked(&server, RootStep::Finish);
    let (replay, attached) = b.attach(&root);
    let replayed = stream(&replay, &root);

    // **(iii) contiguity across the window nobody was watching.** Everything the root said while
    // its own client was dead is here, once each, in order, from the stream's first ordinal.
    let replay_seqs: Vec<u64> = replayed.iter().map(|(s, _)| *s).collect();
    assert_contiguous_from(&replay_seqs, 0, "B's replay of the root's detached window");
    let point = attached.mode.replay_point().records;
    assert_eq!(
        point,
        replayed.len() as u64,
        "the read point counts exactly what has already been delivered"
    );
    assert!(
        attached.mode.is_live(),
        "the root has not exited: {:?}",
        attached.mode
    );
    assert!(
        !replayed
            .iter()
            .any(|(_, p)| p.to_string().contains(ROOT_FINAL_MARKER)),
        "the root's closing turn has not happened — the provider has not been asked for it"
    );
    assert_eq!(
        closing_turns_at_attach, 0,
        "at attach time the provider had **not** been asked for the root's closing turn, so the \
         event asserted on below cannot already exist anywhere — this is the checkable fact §6.1 \
         step 8 requires in place of a sleep"
    );

    // ---- (ii) release, and hear an event that did not exist at attach time -------------------
    gate.release();
    let (between, caused) = b.wait_for_event(|n| match n {
        Note::NodeEvent {
            agent_id, payload, ..
        } => agent_id == &root && payload.to_string().contains(ROOT_FINAL_MARKER),
        _ => false,
    });
    let Note::NodeEvent { agent_seq, .. } = caused else {
        unreachable!()
    };
    assert!(
        agent_seq >= point,
        "an event caused after the attach carries an ordinal past the read point ({agent_seq} < \
         {point})"
    );
    assert!(
        root_turns_asked(&server, RootStep::Finish) > closing_turns_at_attach,
        "the event was caused by a provider request the root made **after** the attach, and the \
         provider's own log is what says so"
    );

    let mut live_seqs: Vec<u64> = stream(&between, &root).iter().map(|(s, _)| *s).collect();
    live_seqs.push(agent_seq);
    assert_contiguous_from(&live_seqs, point, "B's live leg after the replay");

    // ---- **grew after the kill**: liveness, not mere presence in the process table -----------
    assert!(
        events_len(&state, &repo, &root) > at_kill,
        "a zombie is in the process table too. The root's own `events.jsonl` was {at_kill} bytes \
         when its client died and must be longer now — that is the difference between a node that \
         survived and a node that is merely still listed"
    );

    assert_eq!(
        still_the_same_supervisor(&paths, &before),
        Resolution::AliveAndOurs,
        "one supervisor, across the client's whole life and death"
    );

    // The run's own client is dead, so nobody would otherwise say the fleet is finished; this one
    // does, and §5.7's grace is waived for a client that announced itself.
    assert!(
        until(|| journal_nodes(&state, &repo)
            .iter()
            .all(|n| n.state.is_exited())),
        "both nodes reach a terminal record without the dead client: {:?}",
        journal_nodes(&state, &repo)
            .iter()
            .map(|n| (n.agent_id.clone(), n.state))
            .collect::<Vec<_>>()
    );
    b.quit();
    drop(b);
    drop(server);
}

/// The root's pid, from the journal, once the supervisor has written it.
fn until_root_pid(state: &Path, repo: &Path) -> i32 {
    let mut found = None;
    until(|| {
        found = journal_nodes(state, repo)
            .into_iter()
            .find(|n| n.depth() == Some(0))
            .and_then(|n| n.pid);
        found.is_some()
    });
    found.expect("the supervisor journals the root's `Spawned` with a pid at `command.spawn()`")
}

fn root_from_journal(state: &Path, repo: &Path) -> AgentId {
    journal_nodes(state, repo)
        .into_iter()
        .find(|n| n.depth() == Some(0))
        .expect("the run's root is in the journal")
        .agent_id
}

// ------------------------------------------------------------------------------------------
// The root's `Spawned` names a live process
// ------------------------------------------------------------------------------------------

/// **§11 item 28 step 1's rule, applied to the node it had left out.**
///
/// A root's `Spawned` used to be written after the whole turn returned, carrying `pid: None`, on
/// the ground that nothing outside `marion run` could act on the number. The supervisor owning the
/// root is that something. So the record now names a process, and it is written at the instant the
/// process exists rather than at the instant it stops existing — which is what the second half of
/// this test measures: the pid is read **while the root is still running**, from a second process,
/// so a record appended after the run could not be there to read.
///
/// Liveness is `marion_testsupport::liveness` — S15's three-valued `procid` — and never
/// `kill(pid, 0)`, which reports a zombie as alive and would let this pass over a corpse.
#[test]
fn the_roots_spawned_record_names_a_live_process_while_the_root_is_still_running() {
    if !common::script::require_claude_and_codex() {
        return;
    }
    let dir = scratch("client-run-pid");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let gate = TurnGate::holding_from(CHILD_WIRE, 2);
    let server = CannedServer::start_gated(
        Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog: dir.join("provider-requests.jsonl"),
            script: script(),
        },
        Some(Arc::clone(&gate)),
    )
    .expect("the canned provider binds");

    let run = start_run(
        &dir,
        &repo,
        &state,
        &server.base_url(),
        &gate,
        ROOT_MARKER,
        ROOT_BLOCKED_SECS,
    );
    assert!(
        until(|| gate.parked() == 1),
        "the tree is parked, so the root is unambiguously mid-run"
    );

    let nodes = journal_nodes(&state, &repo);
    let root = nodes
        .iter()
        .find(|n| n.depth() == Some(0))
        .expect("the root is in the journal");
    let pid = root.pid.expect(
        "**the whole assertion**: a root's `Spawned` carries the pid of the process marion started. \
         `None` here is the pre-step-6 record, and it is unusable — §6.7's kill has no target and \
         §7.2 cannot tell a node that exists from one that never did",
    );
    assert_eq!(
        liveness(pid),
        Liveness::Alive,
        "the pid the journal names is a process that exists **now**, read from a second process \
         while the run is in flight — so the record cannot have been appended after the run"
    );
    assert_ne!(
        pid,
        std::process::id() as i32,
        "…and it is not this test's own pid, which a `std::process::id()` mutation would write"
    );
    assert!(
        !root.state.is_exited(),
        "the node whose pid this is has not exited: {:?}",
        root.state
    );

    gate.release();
    let mut c = Client::dial(&paths_for(&state, &repo));
    assert!(
        until(|| journal_nodes(&state, &repo)
            .iter()
            .all(|n| n.state.is_exited())),
        "the run finishes"
    );
    c.quit();
    drop(c);
    drop(run);
    drop(server);
}

// ------------------------------------------------------------------------------------------
// A supervisor that cannot start is a refusal
// ------------------------------------------------------------------------------------------

/// A state directory nothing may create anything inside, restored on drop so the scratch guard can
/// remove it.
struct Sealed(PathBuf);

impl Sealed {
    fn new(path: PathBuf) -> Sealed {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&path).expect("the state dir exists before it is sealed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("sealing the state dir");
        Sealed(path)
    }
}

impl Drop for Sealed {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
    }
}

/// **A run whose supervisor cannot start is refused, and journals nothing.**
///
/// This is the test that replaces the one pinning the old behaviour. `bin/marion.rs` used to say, in
/// as many words, *"a supervisor that will not start is reported, not fatal — today … the day
/// `spawn` goes over the socket, this must become a refusal, and the test that pins the current
/// behaviour is named so that whoever changes it has to say so."* This is that change, said out
/// loud: the run now **is** the socket, so a supervisor marion cannot reach is not a lost view, it
/// is a lost run.
///
/// **The absence of a fallback is the assertion, not a detail.** A build that quietly drove the
/// root in this process on failure would exit 0 here and look identical to a healthy run — while
/// leaving a live node no supervisor owned, could kill, or could hand to a re-attaching client.
/// That is why the journal is checked too: a fallback would have written one.
///
/// The supervisor is prevented from starting by sealing the state directory rather than by hiding
/// the binary, because that failure is the operator's real one (a `<state>` they cannot write) and
/// it exercises the same path a full disk would.
#[test]
fn a_run_whose_supervisor_cannot_start_is_refused_and_journals_nothing() {
    let dir = scratch("client-run-nosup");
    let repo = fixture_repo(&dir);
    // Short and outside the scratch tree's sealed part: `socket.rs` overruns `sun_path` under
    // macOS's `/private/var/folders/…`, and a socket path that fell back to `/tmp` would be one the
    // supervisor *could* bind.
    let state = PathBuf::from(format!("/tmp/mnosup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);
    let sealed = Sealed::new(state.clone());

    let out = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args([
            "run",
            "claude",
            "--prompt",
            "this must never launch",
            "--repo",
            &repo.to_string_lossy(),
            "--state-dir",
            &state.to_string_lossy(),
            "--canned",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--timeout",
            "5",
        ])
        .current_dir(&dir)
        .output()
        .expect("marion run starts");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a run with no supervisor did something and called it success\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("this project's supervisor is what runs the root"),
        "the refusal must be in marion's own voice and name what failed:\n{stderr}"
    );
    assert!(
        stderr.contains("no in-process fallback"),
        "and it must say why there is nothing to fall back to:\n{stderr}"
    );
    assert!(
        !stderr.contains("root ") || !stderr.contains(" in /"),
        "nothing may have announced a root:\n{stderr}"
    );

    // **Nothing was journaled**, which is the half a silent fallback would fail. The whole state
    // tree is unwritable, so the check is that marion did not create one somewhere else either.
    drop(sealed);
    let entries: Vec<_> = std::fs::read_dir(&state)
        .expect("the state dir is readable once unsealed")
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    assert!(
        entries.is_empty(),
        "a refused run wrote into <state>: {entries:?}"
    );
    let _ = std::fs::remove_dir_all(&state);
}

// ------------------------------------------------------------------------------------------
// T-crit2 — M2 acceptance criterion 2, and the structure it is stated over
// ------------------------------------------------------------------------------------------

/// How long the supervisor's follower may take to reach the end of a journal that has **stopped
/// growing**. See [`until_within`]: expiry here is a defect report, not a slow machine.
const CATCH_UP: Duration = Duration::from_secs(15);

/// **§9's four things, kept as four**: *"same nodes, same parent edges, same terminal states, same
/// contracts."*
///
/// Four named maps and not one derived blob, because the whole value of the criterion is in *which*
/// of the four broke. A single `assert_eq!` over an opaque struct reports a diff of the whole tree
/// and leaves the reader to work out whether a parent moved or a contract vanished; these report
/// one sentence each.
///
/// Every key is the `AgentId` string, so the two sides are comparable without either of them
/// knowing how the other was built — one is a journal replay, the other is what a client can see
/// (`tree/subscribe`, plus the contract files §9 and `AgentSpawnResult::task_id` tell a client to
/// read for itself).
#[derive(Debug, PartialEq, Eq)]
struct Structure {
    nodes: std::collections::BTreeSet<String>,
    edges: std::collections::BTreeMap<String, Option<String>>,
    states: std::collections::BTreeMap<String, marion_core::node::NodeState>,
    /// `(task_id, requester, status)` per node, sorted — the three fields a `ContractPersisted`
    /// carries and the three a `TaskContract` on disk can be read back for.
    contracts: std::collections::BTreeMap<
        String,
        Vec<(String, String, Option<marion_core::contract::ResultStatus>)>,
    >,
}

impl Structure {
    /// The tree **as the journal records it**, from a replay of an explicit byte prefix.
    fn from_journal(replay: &marion_core::registry::Replay) -> Structure {
        let mut s = Structure::empty();
        for n in replay.nodes() {
            let id = n.agent_id.0.clone();
            s.nodes.insert(id.clone());
            s.edges
                .insert(id.clone(), n.parent_id().map(|p| p.0.clone()));
            s.states.insert(id.clone(), n.state);
            let mut cs: Vec<_> = n
                .contracts
                .iter()
                .map(|c| (c.task_id.0.clone(), c.requester.0.clone(), c.status))
                .collect();
            cs.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
            s.contracts.insert(id, cs);
        }
        s
    }

    /// The tree **as a client reconstructs it**: nodes, parents and states off the socket, and each
    /// node's contracts read from `agents/<agent_id>/contracts/<task_id>.json` — which is the
    /// composition `AgentSpawnResult::task_id` documents, and the only way a client learns a
    /// contract at all (no method on the fifteen returns one).
    fn from_client(
        nodes: &[marion_core::proto::NodeSummary],
        project: &marion_core::paths::ProjectDir,
    ) -> Structure {
        let mut s = Structure::empty();
        for n in nodes {
            let id = n.agent_id.0.clone();
            s.nodes.insert(id.clone());
            s.edges
                .insert(id.clone(), n.parent_id.as_ref().map(|p| p.0.clone()));
            s.states.insert(id.clone(), n.state);
            s.contracts
                .insert(id, contracts_on_disk(project, &n.agent_id));
        }
        s
    }

    fn empty() -> Structure {
        Structure {
            nodes: Default::default(),
            edges: Default::default(),
            states: Default::default(),
            contracts: Default::default(),
        }
    }
}

/// `(task_id, requester, status)` for every contract file under one node's directory.
///
/// Read from the filesystem rather than from the journal on purpose: this is the **independent**
/// half of the contract assertion. A replay that stopped reconstructing `ContractPersisted` would
/// agree with a supervisor that had also stopped — they are the same code — and only the files the
/// run actually wrote can catch that.
fn contracts_on_disk(
    project: &marion_core::paths::ProjectDir,
    id: &AgentId,
) -> Vec<(String, String, Option<marion_core::contract::ResultStatus>)> {
    let dir = project.agent(id).contracts_dir();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for e in entries.filter_map(Result::ok) {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "json") {
            let body =
                std::fs::read(&p).unwrap_or_else(|e| panic!("{} is unreadable: {e}", p.display()));
            let c: marion_core::contract::TaskContract = serde_json::from_slice(&body)
                .unwrap_or_else(|e| panic!("{} is not a TaskContract: {e}", p.display()));
            out.push((
                c.task_id.0,
                c.requester.0,
                c.completion.as_ref().map(|x| x.status),
            ));
        }
    }
    // By the two identifiers, because `ResultStatus` is not `Ord` — and it is not the sort key
    // anyway: a node's contracts are distinguished by their task ids.
    out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    out
}

/// The whole journal's bytes, read from a second process.
fn journal_bytes(state: &Path, repo: &Path) -> Vec<u8> {
    std::fs::read(project(state, repo).journal()).unwrap_or_default()
}

/// **Replay exactly `records` records**, which is what *"the journal as of the replay's own read"*
/// means as a value.
///
/// Records and not bytes, because the read point the supervisor hands back is a record count
/// (`ReplayPoint::records`) and the journal is append-only: the first *n* records of the file are
/// the same *n* records the supervisor folded, whatever has landed since. Fed one line at a time so
/// the prefix can be stopped at a count rather than at an offset the client never told anyone.
fn replay_to(bytes: &[u8], records: u64) -> marion_core::registry::Replay {
    let mut r = marion_core::registry::Replay::default();
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        if r.records as u64 == records {
            break;
        }
        r.extend(line);
    }
    assert_eq!(
        r.records as u64, records,
        "the supervisor says it read {records} records and this journal holds only {}; a read \
         point past the end of the file it is a read of is not a prefix of anything",
        r.records
    );
    r
}

/// §9's four assertions, each named, over one pair of readings.
fn assert_structurally_identical(journal: &Structure, client: &Structure, label: &str) {
    assert_eq!(
        journal.nodes, client.nodes,
        "{label}: **same nodes**. A node in the journal and not in the client's tree is a node the \
         replay dropped; one in the tree and not the journal is a node the supervisor invented"
    );
    assert_eq!(
        journal.edges, client.edges,
        "{label}: **same parent edges**. §7.5 makes a parent immutable, so a difference here is a \
         re-parenting no record authorises"
    );
    assert_eq!(
        journal.states, client.states,
        "{label}: **same terminal states**. The states are the journal's own: a node the client \
         shows running over a journal that records its exit is the failure this clause exists for"
    );
    assert_eq!(
        journal.contracts, client.contracts,
        "{label}: **same contracts**. The client's side is the files the run wrote, so a \
         difference is either a contract the journal records and nothing produced, or one on disk \
         that replay did not reconstruct"
    );
}

/// The agent directories the run actually created — a third reading of *"same nodes"* that shares
/// no code with either of the other two.
fn agent_dirs(state: &Path, repo: &Path) -> std::collections::BTreeSet<String> {
    std::fs::read_dir(project(state, repo).agents_dir())
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn node_at_depth(nodes: &[marion_core::registry::ReplayedNode], depth: u32) -> AgentId {
    nodes
        .iter()
        .find(|n| n.depth() == Some(depth))
        .unwrap_or_else(|| panic!("the run has a node at depth {depth}: {nodes:?}"))
        .agent_id
        .clone()
}

/// **M2 acceptance criterion 2**: the tree a new client reconstructs *is* the journal the
/// supervisor had read at the moment it answered — including everything that happened while no
/// client existed at all.
///
/// # Why this is stated structurally and not as `src_seq` contiguity
///
/// §9 states criterion 2 **per node**, because which form of `src_seq` applies is a property of the
/// surface. A transcript-sourced `interactive` Claude Code node would owe an unbroken
/// `uuid`/`parentUuid` chain; **every node M2 ships — the headless Claude root here and the Codex
/// child — has no ordering evidence at all**, so `Replay::last_src_seq` is `None` on every record
/// in this journal and an ordinal check would be a check of nothing. §4.2 also refuses `agent_seq`
/// contiguity as a substitute: a dropped notification simply never gets a number, so contiguity
/// proves nothing about loss. What is left, and what §9 names the *primary* criterion, is that the
/// replayed tree is structurally identical: same nodes, same parent edges, same terminal states,
/// same contracts.
///
/// # The two things that make it a measurement rather than a tautology
///
/// * **A node is mid-turn when the client dies.** The child's second turn is held at the provider,
///   so at the kill the child is parked with its question asked and the root is inside the `spawn`
///   that is waiting for it. Neither is terminal and neither has a contract.
/// * **A node terminates while nobody is watching.** The hold is released *after* the kill and
///   *before* any new client dials, so the child runs to completion, writes its `TaskContract` and
///   exits with no client in existence. The replay therefore has to pick up a terminal state and a
///   contract that no client ever saw live — which is the difference between replaying a tree and
///   remembering one.
///
/// # And the comparison is against the journal, at the read point the supervisor named
///
/// Not against the pre-kill tree, which §9 is explicit about: agents keep running while the client
/// is dead, so nodes appear, terminate and gain contracts in between, and a diff against a
/// concurrently-evolving tree would fail for reasons that have nothing to do with replay. This test
/// asserts the *change* separately (below) and then compares the client's snapshot to a replay of
/// exactly `read_point.records` records — the number the supervisor handed back with that same
/// snapshot, built under one lock from one read (`handler::subscribe`).
#[test]
fn a_new_clients_tree_is_the_journal_the_supervisor_read_including_the_window_no_client_saw() {
    if !common::script::require_claude_and_codex() {
        return;
    }
    let dir = scratch("client-run-crit2");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let gate = TurnGate::holding_from(CHILD_WIRE, 2);
    let server = CannedServer::start_gated(
        Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog: dir.join("provider-requests.jsonl"),
            script: script(),
        },
        Some(Arc::clone(&gate)),
    )
    .expect("the canned provider binds");

    let mut run = start_run(
        &dir,
        &repo,
        &state,
        &server.base_url(),
        &gate,
        ROOT_MARKER,
        ROOT_BLOCKED_SECS,
    );
    let client_pid = run.pid();
    let paths = paths_for(&state, &repo);

    assert!(
        until(|| gate.parked() == 1),
        "the child's second turn is held, so the tree is parked with its question asked"
    );
    assert!(
        until(|| journal_nodes(&state, &repo).len() == 2),
        "both nodes are in the journal before the kill"
    );

    // ---- the pre-kill tree: two nodes, both mid-turn, no contract anywhere ---------------------
    let before = journal_nodes(&state, &repo);
    let root = node_at_depth(&before, 0);
    let child = node_at_depth(&before, 1);
    let before_child = before.iter().find(|n| n.agent_id == child).unwrap();
    let child_pid = before_child
        .pid
        .expect("the supervisor journals a child's `Spawned` with its pid");
    let root_pid = before
        .iter()
        .find(|n| n.agent_id == root)
        .and_then(|n| n.pid)
        .expect("and the root's");
    assert!(
        !before_child.state.is_exited(),
        "**the mid-turn premise**: at the kill the child is parked at the provider, not finished — \
         {:?}",
        before_child.state
    );
    assert!(
        before_child.contracts.is_empty(),
        "and it has no contract yet, so the one asserted on below cannot already exist"
    );
    assert!(
        before
            .iter()
            .all(|n| !n.state.is_exited() && n.contracts.is_empty()),
        "neither node is terminal while the client is alive: {:?}",
        before
            .iter()
            .map(|n| (n.agent_id.clone(), n.state))
            .collect::<Vec<_>>()
    );

    // ---- the client dies, uncatchably, and only then is the turn released ----------------------
    run.kill_client();
    assert_eq!(
        liveness(client_pid),
        Liveness::Gone,
        "the client is really gone, so everything below happened with no client in existence"
    );
    gate.release();

    // ---- the detached window, gated on facts that are **not** what is asserted about ------------
    //
    // The wait is on the two processes ending and the contract file appearing, deliberately, and
    // not on the journal's own `Exited` records: the states and the contracts in the journal are
    // precisely what the comparison below is *about*, and a test that waited for them would report
    // a broken reconstruction as a timeout instead of as an assertion. Liveness is S15's
    // three-valued reading, so a zombie does not read as alive; the contract file is the run's own
    // artefact, written before the closing bookend (`run::run_spawn`).
    assert!(
        until(|| liveness(child_pid) == Liveness::Gone && liveness(root_pid) == Liveness::Gone),
        "both processes end while no client exists — child {child_pid} is {:?}, root {root_pid} is \
         {:?}",
        liveness(child_pid),
        liveness(root_pid)
    );
    assert!(
        until(|| !contracts_on_disk(&project(&state, &repo), &child).is_empty()),
        "and the child's contract is written with nothing attached to the supervisor: §9 gives the \
         child of a top-level spawn exactly one, and no client ever saw it"
    );

    // ---- a new client, and the journal as of *its* read ----------------------------------------
    let mut b = Client::dial(&paths);
    let mut snapshot = b.tree_at();
    let mut bytes = journal_bytes(&state, &repo);
    let mut total = marion_core::registry::replay(&bytes).records as u64;
    let caught_up = until_within(CATCH_UP, || {
        snapshot = b.tree_at();
        bytes = journal_bytes(&state, &repo);
        total = marion_core::registry::replay(&bytes).records as u64;
        snapshot.read_point.records == total
    });
    assert!(
        caught_up,
        "**the seam.** Both processes are gone, so the journal has stopped growing — at {total} \
         records, of which the supervisor's registry has folded {}. A follower that never reaches \
         the end of a file nobody is writing is serving a tree that is short of its own read point, \
         whether it skipped the record for good or defers it for ever",
        snapshot.read_point.records
    );

    let replayed = replay_to(&bytes, snapshot.read_point.records);
    // **Why the criterion below is structural, as a checked fact rather than a claim.** §9 states
    // criterion 2 per node because the form of `src_seq` is a property of the surface, and neither
    // node here has one: a `Predecessor` check would need an `interactive` transcript and an
    // `Ordinal` gap check an adapter that reports one. This says so out loud — if a future adapter
    // starts carrying source-side evidence, this assertion is what makes someone come back and add
    // the per-node form §9 owes that surface.
    assert!(
        replayed.last_src_seq.is_none(),
        "no record in this journal carries source-side ordering evidence, so structural identity is \
         the whole of the criterion here: {:?}",
        replayed.last_src_seq
    );
    assert!(
        replayed.gaps.is_empty(),
        "marion's own per-writer ordinals are intact — a gap would mean a record was written and \
         lost, which is a different failure from the one below: {:?}",
        replayed.gaps
    );
    let from_journal = Structure::from_journal(&replayed);
    let from_client = Structure::from_client(&snapshot.nodes, &project(&state, &repo));
    assert_structurally_identical(&from_journal, &from_client, "the new client's tree");

    // ---- the window was picked up, and not merely agreed about ---------------------------------
    //
    // Both readings above would be satisfied by a supervisor that had stopped reading the journal
    // at the kill *and* a replay that had done the same. These are what say the tree is the one
    // that includes what happened while no client existed.
    assert_eq!(
        from_client.nodes,
        agent_dirs(&state, &repo),
        "**same nodes**, against a third reading that shares no code with the other two: the \
         directories the run actually created"
    );
    let disk = contracts_on_disk(&project(&state, &repo), &child);
    assert_eq!(
        disk.len(),
        1,
        "the child ran once and wrote one contract while no client existed: {disk:?}"
    );
    assert_eq!(
        disk[0].1, root.0,
        "**same parent edges**, corroborated off the tree entirely: §9 makes the requester of a \
         top-level spawn the root's own `AgentId`, and the file that says so is in the child's own \
         directory"
    );
    assert_eq!(
        from_client.edges.get(&child.0),
        Some(&Some(root.0.clone())),
        "and the client's tree says the same"
    );
    assert_eq!(
        from_client.edges.get(&root.0),
        Some(&None),
        "§9: a root has no parent"
    );
    assert!(
        disk[0].2.is_some(),
        "the contract file records a completion, so the run it is about is over: {disk:?}"
    );
    assert!(
        from_client
            .states
            .get(&child.0)
            .expect("the child is in the tree")
            .is_exited(),
        "**same terminal states**: the child's process is gone and its contract records a \
         completion, so a tree that reports it running is reporting a corpse as a worker — {:?}",
        from_client.states.get(&child.0)
    );
    assert!(
        !before_child.state.is_exited() && before_child.contracts.is_empty(),
        "…and neither the terminal state nor the contract existed when the client died, so replay \
         picked up both from the window it never saw"
    );

    b.quit();
    drop(b);
    drop(run);
    drop(server);
}

// ------------------------------------------------------------------------------------------
// T-crit3 — M2 acceptance criterion 3's second half, "and no untracked live process"
// ------------------------------------------------------------------------------------------

/// The supervisor's own pid, **read out of the journal it wrote**.
///
/// `journal.rs` mints a `WriterId` as `<pid>-<uuid>`, so a journal names the process that wrote it.
/// Taken from there rather than from `ps`, because it is the supervisor that *wrote these records*
/// that criterion 3 is about — a `ps` match on a command line could name a supervisor for another
/// project, or two of them, and this cannot.
fn supervisor_pid(state: &Path, repo: &Path) -> i32 {
    let bytes = journal_bytes(state, repo);
    let line = String::from_utf8_lossy(&bytes);
    let first = line.lines().next().expect("the journal has a record");
    let v: serde_json::Value = serde_json::from_str(first).expect("a record");
    v["writer"]
        .as_str()
        .and_then(|w| w.split('-').next())
        .and_then(|p| p.parse().ok())
        .expect("a writer id is `<pid>-<uuid>`")
}

/// **M2 acceptance criterion 3, second half: after a supervisor SIGKILL, marion can say of every
/// process on the record whether it is still running — and none of them is untracked.**
///
/// # What "untracked" is, and what it is not
///
/// Not *"no live process"*. §7.2 says an `Orphaned` node's process *"may be gone **or still running
/// with marion no longer attached**"*, and a SIGKILLed supervisor leaves its children running on
/// purpose — that is criterion 1, measured two tests up. Reading the criterion as *"nothing is
/// alive"* would make it unsatisfiable by the very scenario it is stated about.
///
/// §11 item 30 says what it does mean: a process *"reparented to pid 1, its wall clock unenforced"*
/// — one **nothing will ever attend to**. So a running orphan is fine, because it is on the record
/// and a restart offers it for resolution; the leak is a live process marion recorded as *finished*,
/// which no timeout, no reaper and no restart will ever look at again. `procid::Claim` is the
/// three-valued reading of that, and this is it end to end.
///
/// # Why the node has to be alive for this to measure anything
///
/// The child's second turn is held at the provider, so at the SIGKILL both nodes are parked with
/// real, live processes. That is what makes the answer interesting: with the supervisor gone,
/// nothing is attached to either of them, and marion has to decide from the journal plus the kernel
/// whether the processes the journal names are still the processes wearing those pids. Were they
/// dead, every resolution would be `Gone` and the test would pass without the start identity ever
/// being consulted — which is exactly the vacuous pass the assertions at the end rule out.
///
/// # Bounded, and it fails by assertion
///
/// Every wait is on a fact: the gate parking, the journal reaching two nodes, the supervisor's pid
/// reading `Gone`. The audit itself is a pure function of the journal and four `sysctl` calls, so
/// there is nothing to wait for at the point the claim is made.
#[test]
fn after_a_supervisor_sigkill_every_process_on_the_record_is_accounted_for() {
    if !common::script::require_claude_and_codex() {
        return;
    }
    let dir = scratch("client-run-crit3");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let gate = TurnGate::holding_from(CHILD_WIRE, 2);
    let server = CannedServer::start_gated(
        Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog: dir.join("provider-requests.jsonl"),
            script: script(),
        },
        Some(Arc::clone(&gate)),
    )
    .expect("the canned provider binds");

    let mut run = start_run(
        &dir,
        &repo,
        &state,
        &server.base_url(),
        &gate,
        ROOT_MARKER,
        ROOT_BLOCKED_SECS,
    );
    assert!(
        until(|| gate.parked() == 1),
        "the child's second turn is held, so both nodes are parked with live processes"
    );
    assert!(
        until(|| journal_nodes(&state, &repo).len() == 2),
        "both nodes are in the journal before the supervisor dies"
    );

    let before = journal_nodes(&state, &repo);
    let pids: Vec<i32> = before.iter().filter_map(|n| n.pid).collect();
    assert_eq!(
        pids.len(),
        2,
        "**the premise: every node here has a real process on the record.** Both `run.rs`'s \
         `announce_started` and `root.rs`'s `launch_inner` write `Spawned` at `command.spawn()` \
         with a pid, so the root counts too — a fact two module docs denied until this test \
         measured it. A tree with fewer pids than nodes would make the audit below cover less than \
         it appears to: {before:?}"
    );
    assert!(
        before.iter().all(|n| !n.state.is_exited()),
        "neither node is terminal, so nothing here is `Gone` for the boring reason: {:?}",
        before.iter().map(|n| n.state).collect::<Vec<_>>()
    );

    // ---- the supervisor dies uncatchably, and its nodes do not ---------------------------------
    let sup = supervisor_pid(&state, &repo);
    // SAFETY: `kill` with a pid read from the journal this test's own supervisor wrote.
    unsafe { kill(sup, 9) };
    assert!(
        until(|| liveness(sup) == Liveness::Gone),
        "the supervisor must really be gone before anything below is attributed to its absence"
    );
    assert!(
        pids.iter().all(|p| liveness(*p) == Liveness::Alive),
        "and its nodes are still running — a SIGKILLed supervisor does not take its fleet with it, \
         which is what makes the question below a real one: {pids:?}"
    );

    // ---- replay, §7.2's marking, then the process audit ----------------------------------------
    let bytes = journal_bytes(&state, &repo);
    let mut tree = marion_core::registry::replay(&bytes);
    let marks = marion_supervisor::restart::apply(&mut tree);
    assert!(
        marks
            .iter()
            .any(|m| m.marking == marion_supervisor::restart::Marking::Orphaned),
        "§7.2's first half: `Live` → `Orphaned`, which is what makes these nodes ones marion says \
         it is *not attached to*: {marks:?}"
    );

    let audit = marion_supervisor::procid::audit(&tree);
    assert_eq!(
        audit.resolved.len(),
        pids.len(),
        "every node with a process on the record is accounted for, and no others: {:?}",
        audit.resolved
    );
    assert!(
        audit.cannot_tell().is_empty(),
        "**the whole point of the recorded start identity.** Before `Spawned` carried one, a live \
         pid could only ever be `cannot-tell`, and §9's claim was unprovable for any fleet that had \
         one: {:?}",
        audit.cannot_tell()
    );
    assert_eq!(
        audit.alive().len(),
        pids.len(),
        "each is positively identified as still being marion's own process — not merely `something \
         is wearing that number`: {:?}",
        audit.resolved
    );
    assert!(
        audit.untracked_and_live().is_empty(),
        "and none of them is untracked: they are orphans, on the record, offered for resolution: \
         {:?}",
        audit.untracked_and_live()
    );
    assert_eq!(
        audit.claim(),
        marion_supervisor::procid::Claim::Holds,
        "§9 criterion 3, second half: **no untracked live process**"
    );

    // ---- and the same tree once the processes really are gone ----------------------------------
    //
    // The pass above is about live processes being identified; this is the other half of the same
    // claim, and it is what stops `Holds` from being a verdict this test can only ever reach one
    // way.
    gate.release();
    let _ = &mut run;
    drop(run);
    drop(server);
    assert!(
        until(|| pids.iter().all(|p| liveness(*p) == Liveness::Gone)),
        "the fixture's sweep ends them: {pids:?}"
    );
    let audit = marion_supervisor::procid::audit(&tree);
    assert!(
        audit.alive().is_empty() && audit.cannot_tell().is_empty(),
        "nothing wears those pids now, and that is definite: {:?}",
        audit.resolved
    );
    assert_eq!(audit.claim(), marion_supervisor::procid::Claim::Holds);
}

// ------------------------------------------------------------------------------------------
// T-crit4 — M2 acceptance criterion 4, the clean quit-and-return
// ------------------------------------------------------------------------------------------

/// §5.7's grace, set **short on purpose** — and it is not what makes clause (i) capable of failing,
/// which is worth stating plainly because the obvious reading is that it is.
///
/// A short grace *cannot* be what gives the clause its force here, for two measured reasons that
/// both point the same way:
///
/// * §7.3.2 waives the grace outright for a client that announced itself
///   (`handler::quit_waived_grace`), so its length is irrelevant from the moment the quit below
///   returns;
/// * and the grace is never consulted anyway. §5.7's exit is gated on **zero clients**, and each
///   node's MCP bridge is itself a socket client of this supervisor (§11 item 28 step 5). While any
///   node is running there is at least one connection, so `idle_exit_eligible` is false regardless
///   of what this constant says, and a build that made it permissive would change nothing.
///
/// What actually makes clause (i) falsifiable is therefore neither the grace nor the eligibility
/// predicate: it is that a supervisor **must not read a clean quit as its own cue to go**. That is
/// the mutation clause (i) is checked against, it walks straight past both mechanisms above, and
/// [`while_the_supervisor_holds`] names it in about a second. Lengthening this constant would not
/// weaken the test — nothing rests on it — and shortening it further would not strengthen it. It is
/// short so that the *suite's* teardown is quick, and that is the whole of its job.
///
/// The consequence for §9 is stated in `MILESTONES.md` rather than hidden here: §5.7's
/// zero-clients-with-a-live-node clause is **structurally unreachable** in marion today, so no test
/// exercises it, and clause (i) is measured on the quit path instead.
const QUIT_GRACE: Duration = Duration::from_millis(300);

/// How long clause (ii) waits for the event the release caused. See [`Client::read_bound`]: this
/// wait's expiry is the defect report, so it gets a budget of its own rather than [`BOUND`].
///
/// **Sized from the measurement, because clause (ii) can only fail by absence.** An event that is
/// never sent cannot be detected except by waiting, so the budget *is* the report's latency and a
/// generous one is not free — it is how long a replay-only build takes to be named. Measured on
/// this platform over five runs, gate release to the event arriving at the client is **5.1–6.0 ms**;
/// five seconds is ~830× that, which is a bound the causal path cannot plausibly cross and a
/// failure that arrives in seconds rather than in [`BOUND`]'s three minutes. Measured against the
/// mutation it exists for: an attach that serves replay and goes quiet is named in ~7 s.
const LIVE_LEG: Duration = Duration::from_secs(5);

/// How long the detached window may take. **Measured at 0.78–0.82 s** over three runs, from the
/// quit to the root parked at its closing turn, so this is ~37× headroom — enough for a loaded
/// machine and short enough that a stalled tree is a named failure rather than a three-minute
/// expiry. See the assertion that uses it.
///
/// It is not the reporting path for clause (i) any more, and that is deliberate:
/// [`while_the_supervisor_holds`] carries the clause *into* both of these waits, so a supervisor
/// that leaves is named in about a second instead of surfacing as this budget running out.
const WINDOW: Duration = Duration::from_secs(30);

/// Start a supervisor this test owns, rather than one `marion run` started as a side effect.
fn start_supervisor(
    state: &Path,
    repo: &Path,
    base_url: &str,
) -> marion_supervisor::detach::Ensured {
    let paths = paths_for(state, repo);
    marion_supervisor::detach::ensure_supervisor(
        &paths,
        &marion_supervisor::detach::Launch {
            program: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
            state_dir: state.to_path_buf(),
            // §2's key, resolved the way production resolves it — `marion.rs` derives the socket,
            // the `Launch` and the `ProjectDir` from this one call.
            project_root: marion_supervisor::socket::project_root(repo),
            idle_grace: QUIT_GRACE,
            auth: marion_harness::Auth::Canned,
            base_url: Some(base_url.to_string()),
        },
    )
    .expect("a supervisor starts")
}

/// **M2 acceptance criterion 4: a clean quit-and-return.**
///
/// # Why this could not be credited to the criterion-1 test
///
/// §9 states this one as *a clean quit-and-return* and says in as many words that the three
/// kill-based criteria *"cannot distinguish"* it: *"a supervisor that only ever replays passes every
/// one of them — and a supervisor that dropped its channels and rebuilt the tree from disk would
/// too."* The criterion-1 test **SIGKILLs** its client, and §7.3.1 exists precisely to distinguish a
/// client that said it was leaving from one that vanished. What that test genuinely proves is the
/// *mechanism* clause (ii) turns on; what it cannot prove is the *scenario*, and in particular
/// clause (i) after a **voluntary** departure — which is the case §5.7's grace governs and the one
/// where a supervisor could legitimately have exited.
///
/// So the client here is not `marion run`. It is a socket client this test owns, which creates the
/// root with `agent/spawn { caller: None }`, leaves through the real `session/quit` path with
/// disposition **(b)**, and is never signalled.
///
/// # The shape, and why one run covers §9's second run too
///
/// The gate holds the **root's third `anthropic` turn**, which is its closing one. Three is
/// measured, not assumed: the root's wire carries a session-title request and two scripted turns,
/// and while the title races the first turn, both precede the closing turn by the whole of the
/// child's run — so the third is deterministically `Finish` even though the first two are not
/// ordered.
///
/// That buys the entire criterion from one run:
///
/// * **at least one node mid-turn** at the quit — the child is running and the root is inside the
///   `spawn` that is waiting for it, so neither is terminal;
/// * **the node keeps producing events while no client exists** — with nothing released, the child
///   runs to completion on its own and the root's tool call returns, all after the quit;
/// * §9's *"run it also with a node that terminated **during** the detached window, whose journal
///   tail must appear in the same attach"* — that is the child, which finishes and exits inside the
///   window and is asserted on below in its own attach;
/// * and the root is left **parked and alive** at its closing turn, which is what leaves something
///   for clause (ii) to hear.
///
/// # No sleeps
///
/// Every wait is on a fact: the journal reaching two nodes, the child reaching a terminal record,
/// the gate parking, the root's `events.jsonl` growing. The ordering clause (ii) rests on is read
/// out of the **provider's own request log** — zero closing turns asked at attach, non-zero after —
/// which is a third party's account, not this test's.
#[test]
fn a_client_that_quits_cleanly_leaves_the_supervisor_running_and_a_new_client_resubscribes() {
    if !common::script::require_claude_and_codex() {
        return;
    }
    let dir = scratch("client-run-crit4");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let gate = TurnGate::holding_from(ROOT_WIRE, 3);
    let server = CannedServer::start_gated(
        Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog: dir.join("provider-requests.jsonl"),
            script: script(),
        },
        Some(Arc::clone(&gate)),
    )
    .expect("the canned provider binds");

    let paths = paths_for(&state, &repo);
    let ensured = start_supervisor(&state, &repo, &server.base_url());
    let before = supervisor_identity(&paths);

    // ---- a client, a tree, and a node mid-turn -------------------------------------------------
    let mut a = Client::dial(&paths);
    // **`Ensured` holds a live connection, and it has to be let go here or the criterion is
    // vacuous.** §5.7's *"the loser dials the winner"* makes that dial the proof that a supervisor
    // is serving, so `ensure_supervisor` hands the caller the socket rather than throwing it away.
    // Held for the length of this test it would mean a client existed throughout — and *"the node
    // keeps producing events while no client exists"* would be false, clause (i) would be asserting
    // about a supervisor that was never at zero clients, and the mutation that lets a supervisor
    // exit on its last client's departure would survive. It did survive, until this line.
    //
    // Dropped **after** A has dialled rather than before, so the count goes two, one, zero with no
    // window in which it is zero by accident and the supervisor could leave for the right reason at
    // the wrong time.
    drop(ensured);
    let root = a.spawn_root(
        &repo,
        &format!("{ROOT_MARKER}: delegate the marker-file task to a child."),
        ROOT_BLOCKED_SECS.parse().expect("a number"),
    );
    assert!(
        until(|| journal_nodes(&state, &repo).len() == 2),
        "the root spawned a child, so there is a tree to be mid-turn"
    );
    let at_quit = journal_nodes(&state, &repo);
    let child = at_quit
        .iter()
        .find(|n| n.depth() == Some(1))
        .expect("a child")
        .agent_id
        .clone();
    assert!(
        at_quit.iter().all(|n| !n.state.is_exited()),
        "**§9's premise: at least one node mid-turn.** Neither node has finished when the client \
         announces its departure: {:?}",
        at_quit.iter().map(|n| n.state).collect::<Vec<_>>()
    );
    let root_events_at_quit = events_len(&state, &repo, &root);
    assert_eq!(
        root_turns_asked(&server, RootStep::Finish),
        0,
        "the root has not asked for its closing turn yet, so nothing asserted on later exists"
    );

    // ---- (b) the departure is *voluntary*, and the client is never signalled --------------------
    // **What "no client exists" does and does not mean here, because it is not what it looks
    // like.** After this the *operator's* client is gone, and that is §9's sense. It is **not**
    // zero connections: each node's MCP bridge is itself a socket client of this supervisor
    // (§11 item 28 step 5), so while the root is running its bridge is connected and §5.7's
    // zero-clients clause is unreachable. Measured, not assumed — instrumenting the accept loop
    // showed the connection count never falling below one across this window.
    //
    // That is why clause (i) is guarded by two independent things, and why the mutation for it
    // has to be a supervisor that treats a *quit* as its own cue to go: making
    // `idle_exit_eligible` permissive changes nothing, because with a node's bridge attached the
    // loop never asks it.
    let outcome = a.quit();
    assert!(
        matches!(outcome, marion_core::proto::QuitOutcome::Detached { .. }),
        "**disposition (b), and the supervisor's own answer says so.** (a) answers `Killed` and \
         (c) answers `ReapedAndDetached`; a build that served one of those in place of (b) would \
         leave a differently-shaped tree behind and this is the cheapest place to notice: {outcome:?}"
    );
    drop(a);

    // ---- the detached window: nobody is watching, and the tree carries on -----------------------
    //
    // Nothing is released here. The child finishes on its own, the root's `spawn` returns, and the
    // root then parks at its closing turn — every byte of it written with no client in existence.
    //
    // **Both waits carry clause (i) with them** — see [`while_the_supervisor_holds`]. The point of
    // the detached window is that the supervisor is still here to drive it, so a supervisor that
    // left is not a slow window and must not be reported as one.
    assert!(
        while_the_supervisor_holds(WINDOW, &paths, &before, || journal_nodes(&state, &repo)
            .iter()
            .any(|n| n.agent_id == child && n.state.is_exited())),
        "§9's second run, folded into this one: a node **terminates during the detached window**"
    );
    assert!(
        while_the_supervisor_holds(WINDOW, &paths, &before, || gate.parked() == 1),
        "the root must reach its closing turn and park there — alive, non-terminal, and with \
         something still to say, which is what clause (ii) needs. **Its own budget, not `BOUND`**: \
         the tree gets here in about a second, and the interesting way to fail is for the root's \
         `spawn` never to return — which is what happens if `node/attach` serves replay only, since \
         the child's own MCP bridge attaches and reads until the closing bookend. At `BOUND` that \
         arrives as a three-minute expiry instead of this sentence"
    );
    assert!(
        events_len(&state, &repo, &root) > root_events_at_quit,
        "**the node kept producing events while no client existed**: the root's own stream was \
         {root_events_at_quit} bytes when its client left and must be longer now"
    );

    // ---- (i) the supervisor is the same process ------------------------------------------------
    assert_eq!(
        still_the_same_supervisor(&paths, &before),
        Resolution::AliveAndOurs,
        "**(i)**, once more at the end of the window and after the whole of it: same pid *and* same \
         kernel start identity, so a reissued pid cannot satisfy it. §7.3.2 waives §5.7's grace for \
         a client that announced itself, so the only thing keeping this process alive is the node \
         that has not finished — which is exactly the clause a supervisor that took its last \
         client's departure as its own cue would get wrong"
    );

    // ---- a new client attaches -----------------------------------------------------------------
    let mut b = Client::dial(&paths);
    let nodes = b.tree();
    assert_eq!(nodes.len(), 2, "the whole tree is there: {nodes:?}");

    // §9: the terminated node's journal tail must appear in the same attach.
    let (child_replay, child_attached) = b.attach(&child);
    let child_stream = stream(&child_replay, &child);
    assert!(
        !child_stream.is_empty(),
        "the child ran and exited entirely inside the detached window, and its stream is served \
         from replay to a client that never saw any of it"
    );
    assert_contiguous_from(
        &child_stream.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        0,
        "B's replay of the child that terminated while nobody was attached",
    );
    assert!(
        !child_attached.mode.is_live(),
        "the child is over, so its attach is a completed reading rather than a subscription — {:?}",
        child_attached.mode
    );
    // **§9's *"whose journal tail must appear in the same attach"*, and `tail` is the load-bearing
    // word.** Everything above this is satisfied by a replay that served the child's opening
    // bookend and then stopped: non-empty holds, contiguity from 0 holds, and the mode is not live
    // either way. What such a replay drops is precisely the records that say the run *ended* — and
    // §7.3.3 names that failure in as many words: *"a stream without a terminal event is
    // indistinguishable from one cut mid-turn"*. So this asserts the **last** record, and asserts
    // it is the terminal bookend rather than merely that more than one arrived.
    let bookends: Vec<Payload> = [child_stream.first(), child_stream.last()]
        .into_iter()
        .map(|e| {
            serde_json::from_value(e.expect("a non-empty stream").1.clone()).expect("a payload")
        })
        .collect();
    assert!(
        matches!(bookends[0], Payload::Lifecycle(Lifecycle::Opened)),
        "the child's replay must start where marion started recording it: {bookends:?}"
    );
    assert!(
        matches!(bookends[1], Payload::Lifecycle(Lifecycle::Exited { .. })),
        "**the journal tail is there**: the child started, ran and finished with nobody attached, \
         and the attach that first shows it to anyone must carry the record of how it ended — \
         {bookends:?}"
    );

    // ---- (iii) the detached window, replayed exactly once ---------------------------------------
    let closing_turns_at_attach = root_turns_asked(&server, RootStep::Finish);
    let (replay, attached) = b.attach(&root);
    let replayed = stream(&replay, &root);
    let replay_seqs: Vec<u64> = replayed.iter().map(|(s, _)| *s).collect();
    assert_contiguous_from(&replay_seqs, 0, "B's replay of the root's detached window");
    let point = attached.mode.replay_point().records;
    assert_eq!(
        point,
        replayed.len() as u64,
        "**(iii)**: the read point counts exactly what has already been delivered, so there is \
         neither a gap nor a repeat where the replay meets the live leg"
    );
    assert!(
        attached.mode.is_live(),
        "the root is parked, not finished, so this is a subscription: {:?}",
        attached.mode
    );
    assert!(
        !replayed
            .iter()
            .any(|(_, p)| p.to_string().contains(ROOT_FINAL_MARKER)),
        "the root's closing turn has not happened, so the event clause (ii) rests on cannot \
         already be in the replay"
    );
    // **The checkable fact §6.1 step 8 requires in place of a sleep, and it is not the same fact
    // the criterion-1 test uses.** There the gate holds a *different* wire, so the root's closing
    // turn has not been asked for at all and the count is zero. Here the gate holds that very turn,
    // and `gate.rs` places the hold **after** the request is logged and before it is answered — on
    // purpose, so the evidence survives on disk while the turn is still parked. So the request is
    // already counted, and what is withheld is the **answer**.
    //
    // That makes the causal statement sharper rather than weaker: at the instant of the attach the
    // provider is holding the root's closing answer, so the event asserted on below cannot exist
    // anywhere — and the count staying at one afterwards proves the event came from releasing that
    // held answer rather than from some later turn.
    assert_eq!(
        closing_turns_at_attach, 1,
        "the root asked for its closing turn and the provider logged it before parking it"
    );
    assert_eq!(
        gate.parked(),
        1,
        "and the answer is still withheld at attach time, so nothing downstream of it exists yet"
    );

    // ---- (ii) an event emitted *after* the attach ----------------------------------------------
    gate.release();
    b.read_bound(LIVE_LEG);
    let (between, caused) = b.expect_event(
        "**(ii) failed.** The provider's held answer was released, so the root emitted its closing \
         event — and this attach never delivered it. An attach that serves replay and then goes \
         quiet is the replay-only implementation §9 says criterion 4 exists to catch, and it is \
         the one assertion in §9 such an implementation fails.",
        |n| match n {
            Note::NodeEvent {
                agent_id, payload, ..
            } => agent_id == &root && payload.to_string().contains(ROOT_FINAL_MARKER),
            _ => false,
        },
    );
    let Note::NodeEvent { agent_seq, .. } = caused else {
        unreachable!()
    };
    assert!(
        agent_seq >= point,
        "**(ii)**: an event caused after the attach carries an ordinal past the read point \
         ({agent_seq} < {point}) — this is the one assertion in §9 a replay-only implementation \
         fails"
    );
    assert_eq!(
        root_turns_asked(&server, RootStep::Finish),
        closing_turns_at_attach,
        "and it came from the answer this test released, not from a turn the root asked for later \
         — the provider's log still shows exactly one closing request, the one that was parked \
         across the attach"
    );

    let mut live_seqs: Vec<u64> = stream(&between, &root).iter().map(|(s, _)| *s).collect();
    live_seqs.push(agent_seq);
    assert_contiguous_from(&live_seqs, point, "B's live leg after the replay");

    assert_eq!(
        still_the_same_supervisor(&paths, &before),
        Resolution::AliveAndOurs,
        "one supervisor, across a client's clean departure and a second client's whole session"
    );

    b.quit();
    drop(b);
    drop(server);
    sweep(&dir.display().to_string());
}

/// **The control on clause (i)'s identity reading: a reissued pid is not the same supervisor.**
///
/// Clause (i) of §9's criterion 4 is *"the supervisor was still the same process — same pid, no
/// restart"*, and the whole force of it lives in the second half. A check that compared pids alone
/// would be satisfied by a supervisor that exited and was replaced by one the kernel happened to
/// hand the same number — the reading that turns *"it outlived its last client"* into *"something
/// is listening"*. That is why [`supervisor_identity`] carries the kernel's start identity beside
/// the pid, and it is what had to survive the move off `ps -o lstart=` onto `procid::read`.
///
/// **The criterion-4 test itself cannot exercise this**, and saying so is why this one exists: pid
/// reuse is the kernel's to schedule, so no test can arrange for a *second* supervisor to be born
/// wearing the first one's number. What it can do is present the identical evidence — a serving
/// supervisor whose pid is the recorded one and whose **start identity is not** — and that is what
/// this does, with a real second reading rather than a fabricated one: the start identity of this
/// test's own process, taken from the same `procid::read` the comparison uses.
///
/// So a build in which the fingerprint degenerated to a pid, or in which `procid::resolve` stopped
/// treating a start-identity mismatch as evidence, fails here — with the supervisor **genuinely
/// alive and genuinely serving**, so nothing about the failure can be blamed on a dead process.
#[test]
fn a_supervisor_wearing_a_reissued_pid_is_not_the_supervisor_that_was_there_before() {
    let dir = scratch("client-run-reissued-pid");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let paths = paths_for(&state, &repo);
    // Nothing spawns a node here, so no provider is ever dialled and the base URL only has to be
    // a URL.
    let ensured = start_supervisor(&state, &repo, "http://127.0.0.1:1");
    let real = supervisor_identity(&paths);

    assert_eq!(
        still_the_same_supervisor(&paths, &real),
        Resolution::AliveAndOurs,
        "the control's control: the supervisor that is serving *is* the one just fingerprinted, so \
         the `Gone` below is about the identity and not about a dead process"
    );

    // The same pid, paired with a start identity that is real and is somebody else's. This is what
    // a reissued pid looks like from the outside, and it is the only way to look at one on purpose.
    let stranger = (
        real.0,
        match procid::read(std::process::id() as i32) {
            procid::Read::Id(start) => start,
            other => panic!("this test's own process must be identifiable: {other:?}"),
        },
    );
    assert_ne!(
        stranger.1, real.1,
        "two different processes must not share a start identity, or the premise is empty"
    );
    assert_eq!(
        still_the_same_supervisor(&paths, &stranger),
        Resolution::Gone,
        "**a process wearing the supervisor's pid is not the supervisor** unless it carries the \
         same start identity. Answered `Gone` rather than a doubt, because a mismatch is evidence: \
         marion knows this is a different process, it is not merely unable to say"
    );

    drop(ensured);
    sweep(&dir.display().to_string());
}
