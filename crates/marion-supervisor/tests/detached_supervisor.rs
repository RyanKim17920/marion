//! What a **detached** supervisor is, measured on the running process rather than described.
//!
//! `detach.rs`'s unit tests check the stage-3 predicate against S15's *shapes*. They deliberately
//! cannot catch the mechanism being wrong, and they say so: S15's rejected `inherit` row —
//! an ordinary spawn whose launcher exits — leads neither its session nor its group and therefore
//! **passes** that predicate while sharing the launcher's session, which is the property S15
//! measured to be dangerous. Only a comparison against the launcher's own identity separates them,
//! and only a real process can be compared.
//!
//! So every assertion here is made against `getsid(2)`, `getpgid(2)` and `ps` on a supervisor this
//! file actually started, and compared against the numbers this test process reads about itself.
//! `ps`'s `sess=` column is not used: S15 records that it prints `0` on macOS, which is why the
//! fixture's own identity table says *"every session number here comes from `getsid(2)`"*.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marion_supervisor::detach::{Launch, ensure_supervisor};
use marion_supervisor::socket::{SocketPaths, SupervisorIdentity, read_identity, socket_paths};

unsafe extern "C" {
    fn getuid() -> u32;
    fn getsid(pid: i32) -> i32;
    fn getpgid(pid: i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}

const SIGKILL: i32 = 9;

/// §5.7's grace, shrunk so ordering can be asserted without waiting out five minutes. It never
/// decides a verdict: every assertion below is a count, an identity or a presence.
const GRACE: Duration = Duration::from_millis(300);

/// A scratch state directory with a **short** path, for `socket.rs`'s reason: `temp_dir()` on macOS
/// is a ~50-byte `/private/var/folders/…` path and a socket under it overruns the 103 bytes a
/// `sun_path` may hold. It doubles as this file's process marker — the path appears verbatim in
/// every stage's argv, so `ps` can name exactly the supervisors this test started and no others.
struct Bed {
    state: PathBuf,
    root: PathBuf,
    paths: SocketPaths,
}

impl Bed {
    fn new(tag: &str) -> Bed {
        let state = PathBuf::from(format!("/tmp/mds-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        std::fs::create_dir_all(&state).expect("scratch state dir");
        let root = state.join("proj");
        std::fs::create_dir_all(&root).expect("project root");
        // SAFETY: reads the calling process's real uid and cannot fail.
        let paths = socket_paths(&state, &root, unsafe { getuid() });
        assert!(
            paths.socket().as_os_str().len() <= 103,
            "this test's own socket path must fit the limit socket.rs is about"
        );
        Bed { state, root, paths }
    }

    fn launch(&self) -> Launch {
        Launch {
            program: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
            state_dir: self.state.clone(),
            project_root: self.root.clone(),
            idle_grace: GRACE,
        }
    }

    /// Every process whose argv names this bed's state directory: the supervisors this test made,
    /// and nothing else on the machine.
    fn supervisors(&self) -> Vec<i32> {
        let out = std::process::Command::new("ps")
            .args(["-A", "-o", "pid=,command="])
            .output()
            .expect("ps runs");
        let needle = self.state.display().to_string();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains(&needle) && l.contains("--detached"))
            .filter_map(|l| l.split_whitespace().next()?.parse().ok())
            .collect()
    }
}

impl Drop for Bed {
    /// Leave no supervisor behind, whatever the test did or failed to do. A test that leaked one
    /// would leak it for five minutes of idle grace, or forever if a node in its journal is
    /// non-terminal — which is §5.7 working as specified and not a reason to skip the cleanup.
    fn drop(&mut self) {
        for pid in self.supervisors() {
            // SAFETY: `kill` with a pid this process just read from `ps`.
            unsafe { kill(pid, SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&self.state);
    }
}

/// Assert that something **happens**, never how long it takes — `registry.rs`'s helper, used here
/// for the same reason and with the same rule: no test in this file may be fixed by widening a bound.
fn until(mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    cond()
}

fn ps_field(pid: i32, field: &str) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", &format!("{field}="), "-p", &pid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// The identity a supervisor published, once it is serving.
fn published(paths: &SocketPaths) -> SupervisorIdentity {
    assert!(
        until(|| read_identity(paths).is_some()),
        "a serving supervisor publishes its identity beside its socket"
    );
    read_identity(paths).expect("just observed")
}

/// **NC — a supervisor that detached is in neither its launcher's process group nor its session, and
/// leads neither of its own.**
///
/// Four of S15's measured relations, checked on a real `marion-supervisor` (`signal.json` →
/// `identity`, and `ctty.json` for the last one):
///
/// | S15 mechanism | shares launcher's session | shares launcher's group | session leader | group leader |
/// |---|---|---|---|---|
/// | `inherit` (**rejected**) | **yes** | **yes** | no | no |
/// | `setsid_leader` (**rejected**) | no | no | **yes** | **yes** |
/// | `setsid_double` (**chosen**) | no | no | no | no |
///
/// All four assertions are needed and none is redundant: the two rejected mechanisms fail on
/// *disjoint* pairs of them, so dropping either pair leaves a test that green-lights a mechanism
/// S15 measured to be worse. That is not a hypothesis — it was checked by building each rejected
/// mechanism and watching which assertions died (see this change's mutation table).
#[test]
fn a_detached_supervisor_shares_neither_session_nor_group_with_its_launcher_and_leads_neither() {
    let bed = Bed::new("identity");
    let ensured = ensure_supervisor(&bed.paths, &bed.launch()).expect("a supervisor starts");
    assert!(
        ensured.started,
        "nothing was listening, so this call started one"
    );

    let id = published(&bed.paths);
    let me = SupervisorIdentity::own();
    assert_ne!(id.pid, me.pid, "the supervisor is a different process");

    // **Measured, not believed.** The identity file is written by the supervisor about itself, so
    // using it as the only evidence would be circular. Everything below comes from the kernel via
    // this process, and the file is then checked *against* those readings — which is what makes the
    // carry itself part of what is under test.
    // SAFETY: both take a pid and answer for a process this test just started.
    let (sid, pgid) = unsafe { (getsid(id.pid), getpgid(id.pid)) };
    assert_ne!(
        sid, -1,
        "getsid failed; the answer is UNKNOWN, never a pass"
    );
    assert_ne!(
        pgid, -1,
        "getpgid failed; the answer is UNKNOWN, never a pass"
    );

    assert_ne!(
        sid, me.sid,
        "S15's `inherit` row: an ordinary spawn keeps the launcher's session, and under a real pty \
         that supervisor showed the launcher's tty in ps"
    );
    assert_ne!(
        pgid, me.pgid,
        "S15's `inherit` row again: sharing the launcher's group is what killed an innocent \
         sibling job in the tree_wide experiment"
    );
    assert_ne!(
        sid, id.pid,
        "S15's ctty tiebreak: a session leader acquires a controlling terminal on the first tty it \
         opens without O_NOCTTY, which is the attachment detaching was for"
    );
    assert_ne!(
        pgid, id.pid,
        "S15's `setsid_double` row: pgid names the middle process, which has already exited"
    );

    // The carry is correct, which is the whole reason it exists: `killpg(pid)` is wrong here and a
    // reader has nothing but this file to learn the right number from.
    assert_eq!(id.sid, sid, "the published sid matches the kernel's");
    assert_eq!(id.pgid, pgid, "the published pgid matches the kernel's");
    assert!(!id.leads_its_session());
    assert!(!id.leads_its_group());

    // Reparented to pid 1: the middle process is gone, which is what makes the two numbers above
    // name a corpse.
    assert!(
        until(|| ps_field(id.pid, "ppid").as_deref().map(str::trim) == Some("1")),
        "a detached supervisor is reparented to pid 1 once its middle stage exits; ppid is {:?}",
        ps_field(id.pid, "ppid")
    );

    // And its group contains **only** it — S15's "group members: itself alone", which is the
    // property that makes `signal_targets`'s own-pgid filter exclude one process instead of a
    // shell's whole job.
    let members: Vec<i32> = String::from_utf8_lossy(
        &std::process::Command::new("ps")
            .args(["-A", "-o", "pid=,pgid="])
            .output()
            .expect("ps runs")
            .stdout,
    )
    .lines()
    .filter_map(|l| {
        let mut f = l.split_whitespace();
        let pid: i32 = f.next()?.parse().ok()?;
        let g: i32 = f.next()?.parse().ok()?;
        (g == pgid).then_some(pid)
    })
    .collect();
    assert_eq!(
        members,
        vec![id.pid],
        "S15 measured the detached supervisor alone in its group"
    );

    drop(ensured);
}

/// **NC — a second client starts no second supervisor.**
///
/// §5.7's start rule read from the ordinary direction rather than the racing one: *"the loser dials
/// the winner rather than erroring or starting a second"*.
///
/// **Both assertions are here because they catch different things, and the mutation check said so.**
/// The `started` flag catches a client that spawns when it should have dialed; the *count of
/// processes* catches a second supervisor that actually got as far as serving. Neither subsumes the
/// other: a client that spawns unconditionally was caught by the flag and **not** by the count,
/// because the redundant stage 3 loses `socket::acquire`'s lock and exits before `ps` can see it —
/// which is the design working, and also exactly why the flag cannot be dropped as "what the
/// implementation believes".
#[test]
fn a_second_client_dials_the_running_supervisor_and_starts_no_second_one() {
    let bed = Bed::new("second");
    let first = ensure_supervisor(&bed.paths, &bed.launch()).expect("a supervisor starts");
    assert!(first.started);
    let id = published(&bed.paths);

    let second = ensure_supervisor(&bed.paths, &bed.launch()).expect("a supervisor is reachable");
    assert!(
        !second.started,
        "one was already listening, so nothing may be spawned"
    );
    assert_eq!(
        bed.supervisors(),
        vec![id.pid],
        "a second supervisor over one project's journal is the outcome §5.7's split was for"
    );
    assert_eq!(
        read_identity(&bed.paths).map(|i| i.pid),
        Some(id.pid),
        "and the one serving is still the first one, not a replacement"
    );

    drop(first);
    drop(second);
}

/// A supervisor's socket, lock and identity are one story, so a client that finds one finds all
/// three — and `socket.rs`'s `Serving::drop` takes the identity with the socket, never leaving a
/// pgid that names nothing behind for a later reader to act on.
#[test]
fn the_published_identity_lives_and_dies_with_the_socket() {
    let bed = Bed::new("lifetime");
    let ensured = ensure_supervisor(&bed.paths, &bed.launch()).expect("a supervisor starts");
    let id = published(&bed.paths);
    assert!(bed.paths.socket().exists());
    assert!(bed.paths.lock().exists());

    drop(ensured);
    // SAFETY: a pid this test started and has just read from its own identity file.
    unsafe { kill(id.pid, SIGKILL) };
    assert!(
        until(|| !alive(id.pid)),
        "the supervisor this test started must be gone before it asserts about what it left"
    );
    // A SIGKILLed supervisor leaves both behind — no `Drop` runs — which is exactly the corpse
    // `socket::acquire`'s lock is what proves stale. Asserting that here is what makes the *clean*
    // case below mean something.
    assert!(bed.paths.identity().exists(), "a kill leaves a corpse");

    let next = ensure_supervisor(&bed.paths, &bed.launch()).expect("the corpse is taken over");
    assert!(next.started);
    assert!(
        until(|| read_identity(&bed.paths).map(|i| i.pid) != Some(id.pid)),
        "the identity is republished by whoever took the lock, not inherited from the corpse"
    );
    drop(next);
}

// ------------------------------------------------------------ §5.7's start, across processes

/// **NC — sixteen racing *processes* produce one supervisor, and only ever one.**
///
/// `socket.rs` already proves this across sixteen **threads**, and that test cannot reach the
/// property §5.7 is actually about: *"two `marion` invocations racing in the same project root
/// resolve to the same path"* — two invocations, which is two processes, with no shared memory, no
/// shared allocator and an `flock` that has to be the whole of the agreement between them.
///
/// **The assertion is not "it converges to one".** A test that only waited for the count to settle
/// would pass on an implementation where two supervisors served in turn, each unlinking the other's
/// socket. So the identity file is sampled throughout the whole race and the assertion is that the
/// number of **distinct** supervisors ever seen serving is one. A takeover would show up as a second
/// pid in that set even if the count were 1 at every instant.
#[test]
fn sixteen_racing_processes_produce_one_supervisor_and_never_a_second() {
    const N: usize = 16;
    let bed = Bed::new("race");
    let launch = bed.launch();

    // Released as close to together as separate processes can be: every one is spawned before any
    // is waited on, so they overlap in the window that matters — between the first `connect` and
    // the winner's `bind`.
    let racers: Vec<_> = (0..N)
        .map(|_| {
            std::process::Command::new(&launch.program)
                .args(launch.argv())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("a racer starts")
        })
        .collect();

    let mut seen: Vec<i32> = Vec::new();
    let mut note = |bed: &Bed| {
        if let Some(id) = read_identity(&bed.paths)
            && !seen.contains(&id.pid)
        {
            seen.push(id.pid);
        }
    };
    for mut r in racers {
        // Every stage 1 must exit, and exit *cleanly*: §5.7 says the loser dials the winner rather
        // than "erroring or starting a second", so a non-zero status here would be a racer that
        // reported failure for losing a race it is specified to lose quietly.
        while r.try_wait().expect("wait").is_none() {
            note(&bed);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            r.wait().expect("wait").success(),
            "a loser must stand down quietly, not report an error"
        );
    }
    assert!(
        until(|| {
            note(&bed);
            bed.supervisors().len() == 1
        }),
        "sixteen racing processes left {:?} supervisors serving one journal",
        bed.supervisors()
    );
    note(&bed);

    assert_eq!(
        seen.len(),
        1,
        "more than one process ever held this project's socket: {seen:?}. A second supervisor \
         holds a second registry over the same journal, which is what §5.1's single-enforcer \
         property and §5.7's start rule exist to prevent."
    );
    assert_eq!(
        bed.supervisors(),
        seen,
        "the one still serving is the one that was serving all along"
    );
    // And it is a supervisor, not merely a process: it answers.
    std::os::unix::net::UnixStream::connect(bed.paths.socket()).expect("the winner is serving");
}

// ---------------------------------------------------------- §5.7's stop, over a real supervisor

/// Three-valued liveness, S15's guard 2 and `marion-testsupport::alive`'s rule: `ESRCH` is dead,
/// success is alive, and **any other errno is not a "dead" answer**. Used here only where a `false`
/// would fail the test anyway, so a wrong reading cannot become a silent pass.
fn alive(pid: i32) -> bool {
    marion_testsupport::alive(pid)
}

/// Write one journal record, as a real writer would: through `marion_core`'s encoder, so a change
/// to the record format breaks this file rather than letting it seed a journal no supervisor reads.
fn seed(path: &Path, seq: u64, kind: marion_core::journal::RecordKind) {
    use std::io::Write;
    let bytes = marion_core::journal::encode(&marion_core::journal::JournalRecord {
        writer: marion_core::journal::WriterId("test".into()),
        seq,
        ts: marion_core::encoding::SystemTime::from_unix_millis(1_000 + seq),
        mono_ns: seq,
        provenance: marion_core::ir::Provenance::marion(),
        src_seq: None,
        kind,
    })
    .expect("a record encodes");
    std::fs::create_dir_all(path.parent().expect("a journal has a directory")).expect("dir");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("journal")
        .write_all(&bytes)
        .expect("append");
}

/// A node the journal describes as **running**: intent, confirmation, and a state that is not
/// terminal. Three records and not one, because `resident_reason` distinguishes `SpawnOutstanding`
/// from `NonTerminalNode` and a test that seeded only the intent would be asserting about the
/// wrong clause of §5.7's exclusion list.
fn a_running_node(path: &Path, agent: &str) {
    use marion_core::journal::{RecordKind, SpawnIntent, Spawned, StateChanged};
    let id = marion_core::contract::AgentId(agent.into());
    seed(
        path,
        0,
        RecordKind::SpawnIntent(SpawnIntent {
            agent_id: id.clone(),
            parent_id: None,
            agent_type: "codex-impl".into(),
            harness: marion_core::harness::Harness::Codex,
            depth: 0,
            task_id: None,
        }),
    );
    seed(
        path,
        1,
        RecordKind::Spawned(Spawned {
            agent_id: id.clone(),
            harness_version: "0.146.0".into(),
            model: None,
            pid: Some(1),
        }),
    );
    seed(
        path,
        2,
        RecordKind::StateChanged(StateChanged {
            agent_id: id,
            state: marion_core::node::NodeState::Running,
        }),
    );
}

/// The same node, having exited: nothing in §5.7's exclusion list is left holding.
fn a_finished_node(path: &Path, agent: &str) {
    use marion_core::contract::{ExitStatus, ProcessExit};
    use marion_core::journal::{Exited, RecordKind};
    a_running_node(path, agent);
    seed(
        path,
        3,
        RecordKind::Exited(Exited {
            agent_id: marion_core::contract::AgentId(agent.into()),
            status: ExitStatus::Ok,
            exit: ProcessExit {
                code: Some(0),
                signal: None,
                description: "the node finished".into(),
            },
        }),
    );
}

/// Ask a running supervisor to quit, and read what it answered.
fn session_quit(paths: &SocketPaths) -> marion_proto::QuitOutcome {
    use std::io::{BufRead, Write};
    let mut c = std::os::unix::net::UnixStream::connect(paths.socket()).expect("dial");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let frame = marion_proto::Frame::Request(marion_proto::Request::new(
        marion_proto::RequestId::Number(1),
        marion_proto::Call::SessionQuit(marion_proto::params::SessionQuitParams {
            disposition: marion_proto::QuitDisposition::DetachAll,
        }),
    ));
    c.write_all(frame.to_line().as_bytes()).unwrap();
    c.flush().unwrap();
    let mut line = String::new();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    assert!(
        r.read_line(&mut line).expect("a frame arrives") > 0,
        "closed"
    );
    let marion_proto::Frame::Response(resp) =
        marion_proto::Frame::from_line(&line).expect("well-formed")
    else {
        panic!("a request is answered by a response")
    };
    let marion_proto::Outcome::Result(body) = resp.outcome else {
        panic!("session/quit was refused: {line}")
    };
    let marion_proto::MethodResult::SessionQuit(r) = marion_proto::Method::SessionQuit
        .decode_result(&body)
        .expect("the result decodes")
    else {
        panic!("a quit answers a quit result")
    };
    r.outcome
}

fn journal_tags(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            v.get("kind")?
                .as_object()?
                .keys()
                .next()
                .map(|k| k.to_string())
        })
        .collect()
}

/// **NC — a detached supervisor with a non-terminal node refuses to exit, and says which clause of
/// §5.7 held it.**
///
/// This is the whole reason the supervisor is a separate process: *"a fleet that dies when the last
/// window closes is a fleet the operator can never walk away from"*. `handler.rs` already asserts
/// the predicate in-process; what only a real detached supervisor can show is that the **process**
/// is still there after its only client has gone and the grace has elapsed several times over.
///
/// The wait is not a timeout that could be widened to fix a flake: it is the opposite direction.
/// Waiting *longer* only strengthens the claim, and the assertion is `alive`, never `alive within N`.
#[test]
fn a_supervisor_holding_a_non_terminal_node_refuses_to_exit_when_its_last_client_leaves() {
    let bed = Bed::new("resident");
    let journal = marion_core::paths::ProjectDir::new(&bed.state, &bed.root).journal();
    a_running_node(&journal, "root");

    let ensured = ensure_supervisor(&bed.paths, &bed.launch()).expect("a supervisor starts");
    let id = published(&bed.paths);

    let outcome = session_quit(&bed.paths);
    let marion_proto::QuitOutcome::Detached { supervisor, .. } = outcome else {
        panic!("DetachAll answers Detached")
    };
    assert_eq!(
        supervisor,
        marion_proto::SupervisorDisposition::Resident(
            marion_proto::ResidentReason::NonTerminalNode
        ),
        "the supervisor must name the clause of §5.7's exclusion list that holds it, not merely \
         decline"
    );

    // Its last client is gone: the quit closed that connection, and this was the only other one.
    drop(ensured);
    // Several graces, so "it had not got round to it yet" is not an available reading.
    std::thread::sleep(GRACE * 5);
    assert!(
        alive(id.pid),
        "§5.7: a supervisor with zero clients and at least one non-terminal node MUST keep running"
    );
    assert!(
        !journal_tags(&journal).contains(&"SupervisorExited".to_string()),
        "and it must not have journaled a departure it did not make"
    );
    assert_eq!(
        read_identity(&bed.paths).map(|i| i.pid),
        Some(id.pid),
        "still the same process, still serving"
    );
}

/// **A known gap, pinned rather than described: a journal line the registry cannot parse makes the
/// supervisor immortal.**
///
/// `registry.rs` stops following a journal at an unparsable line and says why it must — *"an
/// authority may not keep serving a tree from a file it no longer recognises"*. That is the right
/// call for a registry. Its consequence one level up is not in §5.7 at all: the supervisor keeps
/// answering §5.7's exit predicate from a tree frozen **before** the records that would have
/// cleared it, so it reports a node as non-terminal forever and nothing short of a signal can end
/// it. §5.7's exclusion list has four clauses and *"the registry stopped following"* is not among
/// them.
///
/// This test asserts the **current** behaviour, deliberately, because the alternative was leaving it
/// as a sentence in a report. If a later change gives §5.7 a clause for a stopped registry, this
/// test fails and whoever changed it has to decide what the new answer is — which is the point.
/// Nothing here endorses the behaviour: `tests/run_stream.rs` reaps a supervisor stranded exactly
/// this way, and cites this test for why it has to.
#[test]
fn a_journal_the_registry_cannot_parse_freezes_the_exit_predicate_and_nothing_clears_it() {
    let bed = Bed::new("frozen");
    let journal = marion_core::paths::ProjectDir::new(&bed.state, &bed.root).journal();
    a_running_node(&journal, "root");

    let ensured = ensure_supervisor(&bed.paths, &bed.launch()).expect("a supervisor starts");
    let id = published(&bed.paths);
    // The supervisor has already folded the running node, so what follows is about the *tail* and
    // not about a supervisor that never read anything.
    let marion_proto::QuitOutcome::Detached { detached, .. } = session_quit(&bed.paths) else {
        panic!("DetachAll answers Detached")
    };
    assert_eq!(detached, [marion_core::contract::AgentId("root".into())]);

    {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&journal)
            .expect("journal")
            .write_all(b"this is not a journal record\n")
            .expect("append");
    }
    // …and then the record that *would* have released the supervisor, which it will never read.
    a_finished_node(&journal, "root");

    let marion_proto::QuitOutcome::Detached { supervisor, .. } = session_quit(&bed.paths) else {
        panic!("DetachAll answers Detached")
    };
    assert_eq!(
        supervisor,
        marion_proto::SupervisorDisposition::Resident(
            marion_proto::ResidentReason::NonTerminalNode
        ),
        "the exit record is on disk and the supervisor cannot see it: this is the gap, not a bug \
         in this test"
    );
    drop(ensured);
    std::thread::sleep(GRACE * 3);
    assert!(
        alive(id.pid),
        "and no amount of waiting clears it, which is why the reap in run_stream.rs exists"
    );
}

/// **NC — a supervisor with nothing left journals its exit *and then* goes, leaving neither a socket
/// nor a pgid behind.**
///
/// The complement of the test above, and the half that stops that one from passing vacuously: a
/// supervisor that never exited under any circumstances would satisfy "refuses to exit" perfectly.
///
/// Four assertions in **order**, because §5.7 makes the order the property: the exit record is
/// written *"at the moment the supervisor decides to go, not as a best-effort epitaph"*, so a
/// process that vanished without one is indistinguishable from one that died — which is the exact
/// distinction the record exists to make. This also closes the clean half of the identity's
/// lifetime: a `SIGKILL` leaves a corpse (asserted elsewhere in this file), and a decision leaves
/// nothing.
#[test]
fn a_supervisor_with_nothing_left_journals_its_exit_and_leaves_no_socket_identity_or_process() {
    let bed = Bed::new("exiting");
    let journal = marion_core::paths::ProjectDir::new(&bed.state, &bed.root).journal();
    a_finished_node(&journal, "root");

    let ensured = ensure_supervisor(&bed.paths, &bed.launch()).expect("a supervisor starts");
    let id = published(&bed.paths);

    let outcome = session_quit(&bed.paths);
    let marion_proto::QuitOutcome::Detached { supervisor, .. } = outcome else {
        panic!("DetachAll answers Detached")
    };
    assert_eq!(
        supervisor,
        marion_proto::SupervisorDisposition::Exiting,
        "nothing in §5.7's exclusion list holds, so the supervisor may go"
    );
    drop(ensured);

    assert!(until(|| !alive(id.pid)), "it said it was going, so it goes");
    assert!(
        journal_tags(&journal).contains(&"SupervisorExited".to_string()),
        "§5.7: the exit MUST be journaled, so a later reader can tell 'finished and left' from \
         'died': {:?}",
        journal_tags(&journal)
    );
    assert!(
        !bed.paths.socket().exists(),
        "a clean exit takes its socket with it"
    );
    assert!(
        !bed.paths.identity().exists(),
        "and its identity, so nothing is left naming a pgid that no longer exists"
    );
    assert!(
        bed.paths.lock().exists(),
        "the lock file persists on purpose (socket.rs): unlinking it would let two processes flock \
         two different inodes at one path and both conclude they are alone"
    );

    // And the project is one a supervisor can start in again — which is the only way to tell a
    // clean exit from a wedged one from outside.
    let next = ensure_supervisor(&bed.paths, &bed.launch()).expect("the project is startable");
    assert!(next.started);
    assert!(until(
        || read_identity(&bed.paths).map(|i| i.pid) != Some(id.pid)
    ));
    drop(next);
}
