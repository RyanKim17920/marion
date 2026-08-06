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

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marion_supervisor::detach::{Launch, ensure_supervisor, ensure_supervisor_within};
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

    /// Every process whose argv names this bed's state directory, **whichever stage it is**.
    ///
    /// Stage 1 and stage 2 are ordinarily too short-lived to see, and that is exactly why they have
    /// to be looked for: a stage 1 blocked in `wait` on a stage 2 that never exits is invisible to a
    /// filter on `--detached`, so it is neither asserted about nor cleaned up, and it holds a
    /// descriptor on the launcher's stderr for as long as it lives. The needle is the state
    /// directory, which every stage carries in its argv by construction ([`Launch::argv`]).
    fn processes(&self) -> Vec<i32> {
        let out = std::process::Command::new("ps")
            .args(["-A", "-o", "pid=,command="])
            .output()
            .expect("ps runs");
        let needle = self.state.display().to_string();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains(&needle))
            .filter(|l| l.contains("serve"))
            .filter_map(|l| l.split_whitespace().next()?.parse().ok())
            .collect()
    }

    /// Only the **supervisors** — stage 3. Used where the claim is about how many processes hold
    /// this project's socket, which stages 1 and 2 never do.
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
    ///
    /// **Every stage, not only stage 3**: see [`Bed::processes`].
    fn drop(&mut self) {
        for pid in self.processes() {
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

/// Every test that ends by letting go of its last connection asserts this rather than leaving the
/// process for [`Bed::drop`]'s `SIGKILL`. A fixture that sweeps up is a safety net; a fixture that
/// is the *only* thing ending a supervisor is a leak the assertions cannot see, and this file spent
/// a revision in exactly that state because §5.7's exit was gated on an explicit quit having
/// arrived. It is not any more, so an empty supervisor with no clients goes on its own — and a test
/// that says so is what keeps it that way.
fn goes_on_its_own(bed: &Bed) {
    assert!(
        until(|| bed.supervisors().is_empty()),
        "§5.7: zero clients, zero non-terminal nodes — this supervisor must leave without being \
         killed by the fixture, and {:?} did not",
        bed.supervisors()
    );
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
    goes_on_its_own(&bed);
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
    goes_on_its_own(&bed);
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
    goes_on_its_own(&bed);
}

/// **NC — a client does not report that it reached a supervisor because a dead one's socket
/// answered.**
///
/// This is the flake in this file made deterministic, and it was a real defect rather than a test
/// artifact. `socket.rs` measured that `connect` to a listener's path keeps succeeding for a window
/// after that listener's process is gone, so a client that took a successful dial as its answer
/// would return a stream to nobody: `marion run` would report it had attached to a fleet that no
/// process is enforcing, and the corpse would still be there for the next client. The race is not
/// what is posed here — the *state* is, and it is a state no supervisor can ever be in, because a
/// supervisor binds only while holding the lock.
#[test]
fn a_client_that_dials_a_dead_supervisors_socket_starts_a_live_one() {
    let bed = Bed::new("answering");
    std::fs::create_dir_all(bed.paths.dir()).expect("the project's directory");
    let impostor =
        std::os::unix::net::UnixListener::bind(bed.paths.socket()).expect("a socket that answers");
    assert!(
        std::os::unix::net::UnixStream::connect(bed.paths.socket()).is_ok(),
        "the fixture only means something if the dial really does succeed"
    );

    let ensured = ensure_supervisor(&bed.paths, &bed.launch()).expect("a supervisor is startable");
    assert!(
        ensured.started,
        "the dial reached a socket with no supervisor behind it, so one had to be started"
    );
    let id = published(&bed.paths);
    assert_eq!(bed.supervisors(), vec![id.pid]);
    drop(impostor);
    drop(ensured);
    goes_on_its_own(&bed);
}

/// **NC — a supervisor whose project directory is removed under it stands down instead of becoming
/// immortal.**
///
/// `socket.rs` says what cannot be prevented and why: the `flock` that makes one supervisor per
/// project is an agreement about an **inode**, and removing `<state>/<hash>` removes the name both
/// processes would have had to reach it through. The evicted supervisor keeps a lock nobody can
/// contend, on a socket nobody can dial, over a journal it can no longer append its own exit to —
/// so §5.7's *"the exit MUST be journaled"* has nowhere to go, and nothing short of a signal ends it.
///
/// This asserts the process-level consequence, which no unit test can reach: it goes, and what it
/// left is a project a supervisor can be started in again. The wait is `until`, never a duration —
/// waiting longer only strengthens the claim.
#[test]
fn a_supervisor_whose_state_directory_is_removed_stands_down_rather_than_serving_on() {
    let bed = Bed::new("evicted");
    let ensured = ensure_supervisor(&bed.paths, &bed.launch()).expect("a supervisor starts");
    let id = published(&bed.paths);
    assert_eq!(bed.supervisors(), vec![id.pid]);

    // Exactly what defeats the lock: the pathnames survive the removal, the inodes do not.
    std::fs::remove_dir_all(bed.paths.dir()).expect("remove the project's directory");
    assert!(
        until(|| !alive(id.pid)),
        "an evicted supervisor holds a lock nobody can contend and a socket nobody can dial; it \
         must not go on holding them for the rest of the machine's uptime"
    );
    drop(ensured);

    // And what it left is startable, which is the only way to check the eviction from outside.
    let next = ensure_supervisor(&bed.paths, &bed.launch()).expect("the project is startable");
    assert!(next.started);
    let after = published(&bed.paths);
    assert_ne!(after.pid, id.pid, "a new supervisor, not the evicted one");
    drop(next);
    goes_on_its_own(&bed);
}

// ------------------------------------------------- §5.7's start, when a stage dies half way

/// A stand-in for `marion-supervisor` that a test can make fail at a chosen point.
///
/// Killing a stage at the instant that matters is a race a test cannot win, so the *outcome* of the
/// kill is produced deterministically instead: a shell script in the launcher's place, which does
/// exactly what the real stage would have done and then reports whatever the test asked for. It
/// stands in for **stage 1 only** — `main.rs` derives each stage's program from `current_exe`, so
/// every process after the first is the real binary.
fn wrapper(bed: &Bed, name: &str, body: &str) -> PathBuf {
    let real = env!("CARGO_BIN_EXE_marion-supervisor");
    let path = bed.state.join(name);
    std::fs::write(&path, format!("#!/bin/sh\nREAL='{real}'\n{body}\n"))
        .expect("write the stand-in");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// **NC — a stage that dies after the supervisor is on its way is not reported as a failure to
/// start.**
///
/// Two of the three kill windows produce exactly this: kill stage 1 once it has spawned stage 2,
/// or kill stage 2 once it has spawned stage 3, and the launcher's `wait` returns a non-zero status
/// while a perfectly good supervisor is binding the socket. A client that turns that status into an
/// error has told its operator that nothing started, and left a supervisor running to contradict it
/// — which is worse than either outcome alone, because the *next* invocation will find the socket
/// answering and the two reports cannot both be true.
///
/// The status is what is observable to the launcher, so the status is what the stand-in produces.
#[test]
fn a_supervisor_that_started_is_reported_as_started_even_if_its_launcher_stage_failed() {
    let bed = Bed::new("stagedied");
    // Do the real work, then report the failure a killed stage 1 or stage 2 would have reported.
    let program = wrapper(&bed, "dying-stage-one", "\"$REAL\" \"$@\"\nexit 1");
    let launch = Launch {
        program,
        ..bed.launch()
    };

    let ensured = ensure_supervisor(&bed.paths, &launch)
        .expect("a supervisor is serving, whatever its launcher stage said on the way out");
    assert!(ensured.started, "this call is what brought it into being");
    let id = published(&bed.paths);
    assert_eq!(
        bed.supervisors(),
        vec![id.pid],
        "exactly one supervisor, and it is the one that published"
    );
    drop(ensured);
    goes_on_its_own(&bed);
}

/// **NC — a supervisor that never arrived is started again, rather than waited out.**
///
/// The mirror image of the case above, and it is the one the "exactly once" comment on
/// [`ensure_supervisor`]'s spawn made unreachable: if stage 3 dies before it binds — an `exec`
/// failure, an OOM kill, a stage-2 refusal — then `started` is true, nothing is listening, the lock
/// is free, and the client sits out its whole bound to report `NotReachable` about a project it
/// could have started a supervisor in at any point.
///
/// Determinism without a race: the stand-in exits **0 having started nothing** the first time it is
/// asked, which is precisely what the launcher observes when stage 3 dies immediately, and does the
/// real thing every time after. The bound below never decides the verdict — with a retry the dial
/// succeeds in milliseconds, and without one no bound whatsoever produces a supervisor.
#[test]
fn a_client_whose_first_supervisor_never_arrived_starts_another_one() {
    let bed = Bed::new("stillborn");
    let marker = bed.state.join("first-attempt");
    let program = wrapper(
        &bed,
        "stillborn-stage-one",
        &format!(
            "if [ ! -f '{m}' ]; then : > '{m}'; exit 0; fi\nexec \"$REAL\" \"$@\"",
            m = marker.display()
        ),
    );
    let launch = Launch {
        program,
        ..bed.launch()
    };

    let ensured = ensure_supervisor_within(&bed.paths, &launch, Duration::from_secs(20))
        .expect("nothing was serving and the lock was free, so another supervisor was startable");
    assert!(ensured.started);
    assert!(marker.exists(), "the first attempt really did happen");
    let id = published(&bed.paths);
    assert_eq!(bed.supervisors(), vec![id.pid]);
    drop(ensured);
    goes_on_its_own(&bed);
}

/// **NC — a stage-3 failure on a fresh project is recorded, not discarded.**
///
/// `spawn_stage_three` redirects the supervisor's stderr into `supervisor.log` beside the socket,
/// and on a project nothing has served yet that directory does not exist — so the `open` fails, the
/// redirect silently degrades to `/dev/null`, and the one process whose failures nobody is watching
/// becomes the one process that cannot report them. Two phases, because the two halves fail
/// separately: the log must be **created** on a fresh project, and it must **carry** what stage 3
/// said.
///
/// **Stage 1 is invoked directly, exactly once**, rather than through [`ensure_supervisor`]. That is
/// not a shortcut, it is the difference between testing the mechanism and testing the client's
/// retry policy: a client that starts a second chain finds the directory the first chain's
/// supervisor created, so its stage 2 opens the log whatever stage 2 does about directories, and
/// the assertion passes for a reason that has nothing to do with the property. That is not a
/// hypothesis — this test passed against a build with the fix removed until it was written this way.
#[test]
fn a_fresh_projects_stage_three_writes_its_failures_to_the_projects_log() {
    let bed = Bed::new("logged");
    let launch = bed.launch();
    let log = marion_supervisor::detach::log_path(&launch);
    assert!(
        !log.exists() && !bed.paths.dir().exists(),
        "the point of the test is a project nothing has served yet"
    );

    // One chain, and it is waited for: stage 1 exits when stage 2 does, and stage 2 exits once it
    // has spawned the supervisor, so this returns with stage 3 already on its way.
    let stage_one = |launch: &Launch| {
        std::process::Command::new(&launch.program)
            .args(launch.argv())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .expect("stage 1 runs")
    };
    assert!(stage_one(&launch).success(), "stage 1 reported failure");
    let id = published(&bed.paths);
    assert!(
        log.exists(),
        "stage 2 opened the log before anything created its directory, so the supervisor's stderr \
         went to /dev/null: {}",
        log.display()
    );

    // Phase two: a stage 3 that cannot bind. A read-only project directory is a bind failure with a
    // sentence attached, and the sentence has to arrive somewhere an operator can read it — stage
    // 3's stderr is the only place it can.
    unsafe { kill(id.pid, SIGKILL) };
    assert!(until(|| !alive(id.pid)));
    let _ = std::fs::remove_file(bed.paths.socket());
    let _ = std::fs::remove_file(bed.paths.identity());
    std::fs::set_permissions(bed.paths.dir(), std::fs::Permissions::from_mode(0o500))
        .expect("a project directory nothing can create a socket in");

    assert!(stage_one(&launch).success(), "the failure is stage 3's");
    assert!(
        until(|| std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("marion-supervisor:")),
        "stage 3 could not bind and said so; the log is where that sentence has to be: {:?}",
        std::fs::read_to_string(&log)
    );
    assert!(
        bed.supervisors().is_empty(),
        "a stage 3 that could not bind is not a supervisor and must not still be running: {:?}",
        bed.supervisors()
    );
    std::fs::set_permissions(bed.paths.dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
}

// ------------------------------------------------------------ §5.7's start, across processes

/// **NC — sixteen racing *processes* produce one supervisor, and only ever one.**
///
/// `socket.rs` already proves this across sixteen **threads**, and that test cannot reach the
/// property §5.7 is actually about: *"two `marion` invocations racing in the same project root
/// resolve to the same path"* — two invocations, which is two processes, with no shared memory, no
/// shared allocator and an `flock` that has to be the whole of the agreement between them.
///
/// **The assertion is not "it converges to one", and it is not sampling either.** A test that only
/// waited for the count to settle would pass on an implementation where two supervisors served in
/// turn, each unlinking the other's socket. The earlier revision reached for that by counting
/// distinct pids in the identity file — but it sampled only while it happened to be waiting on a
/// launcher, at 2 ms, so a supervisor that published and vanished between two samples was invisible
/// and the claim *"ever seen serving"* was really *"seen whenever this thread looked"*.
///
/// The sampling is kept and its claim is written down as what it is. The load-bearing assertion is
/// the one below it, which does not sample at all: **the pid serving at the end is the first pid
/// ever published.** Both are single observations, so no rate can hide anything between them — a
/// takeover in either direction makes them differ, whatever the sampler saw.
///
/// **Why a non-terminal node is seeded before the race starts.** This test used to race the
/// supervisor's own correct behaviour and lose. Nothing here holds a client: the racers dial, then
/// exit, and every assertion below happens after the last of them has been waited on. That leaves
/// the winner with zero clients and an empty exclusion list, so §5.7 *permits* it to journal a
/// `SupervisorExited` and leave — and on a loaded machine sixteen processes take longer to finish
/// than this file's 300 ms grace, so it did. Both halves then failed for the same reason and looked
/// like two different bugs: `bed.supervisors()` read `[]` when the winner left mid-assertion, and
/// `seen` grew to two pids when it left mid-*race* and a later racer legitimately took the socket
/// over. Neither was a second enforcer; both were one enforcer that had already gone.
///
/// The fix is not a longer grace or a wider bound — those measure the machine. §5.7's exclusion
/// list is the structural lever: **a supervisor holding a non-terminal node must not exit**, at
/// every instant and under any load. So the journal names a running node before the first racer is
/// spawned, which makes the idle exit unreachable rather than unlikely, and the node is finished at
/// the end so [`goes_on_its_own`] still proves the winner leaves of its own accord. What the race
/// measures is unchanged: the sixteen still contend for the same lock and the same path, and both
/// takeover assertions still stand exactly as written.
#[test]
fn sixteen_racing_processes_produce_one_supervisor_and_never_a_second() {
    const N: usize = 16;
    let bed = Bed::new("race");
    let launch = bed.launch();

    // §5.7's exclusion list, armed before anything races: whoever wins may not idle-exit while the
    // test is still reading it. A real process, so the journal names something that is really
    // running (see [`a_running_node`]).
    let journal = marion_core::paths::ProjectDir::new(&bed.state, &bed.root).journal();
    let mut held = std::process::Command::new("sleep")
        .arg("120")
        .spawn()
        .expect("a real process to be non-terminal about");
    a_running_node(&journal, "root", held.id() as i32);

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

    // The first publisher, observed before anything is waited on. Half of the takeover control,
    // and the only half that does not depend on a sampling rate.
    assert!(
        until(|| read_identity(&bed.paths).is_some()),
        "one of sixteen racers must publish an identity"
    );
    let first_published = read_identity(&bed.paths).expect("just observed").pid;

    let mut seen: Vec<i32> = vec![first_published];
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
    assert_eq!(
        read_identity(&bed.paths).map(|i| i.pid),
        Some(first_published),
        "the process serving at the end is the first one that ever published: two observations, \
         no sampling between them, so a takeover cannot fall through the gap"
    );
    // And it is a supervisor, not merely a process: it answers.
    std::os::unix::net::UnixStream::connect(bed.paths.socket()).expect("the winner is serving");

    // The one clause holding it is cleared, and only then. The winner of a sixteen-way race is an
    // ordinary supervisor in every other respect, so it must leave on its own once nothing in
    // §5.7's exclusion list is left — which is also the control that the seeding above was what
    // kept it, and not some other reason it could not exit at all.
    assert!(
        !journal_tags(&journal).contains(&"SupervisorExited".to_string()),
        "the winner has a non-terminal node and must not have journaled a departure: {:?}",
        journal_tags(&journal)
    );
    let _ = held.kill();
    let _ = held.wait();
    a_node_exited(
        &journal,
        "root",
        "the node finished once the race was decided",
    );
    goes_on_its_own(&bed);
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
///
/// **The pid is a parameter and the residency test passes a real one.** These records are the whole
/// of what a supervisor knows about a node today — it does not hold the `Child`, the pipes or the
/// channel; `marion run` does, and §11 item 28 is that gap — so a test seeding pid 1 was measuring
/// that stale bytes keep a process alive, which is true and is not §5.7's claim. Handing it the pid
/// of a process the test really started does not close the gap, but it stops the fixture from
/// asserting past it.
fn a_running_node(path: &Path, agent: &str, pid: i32) {
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
            pid: Some(pid),
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

/// The record that clears [`a_running_node`] from §5.7's exclusion list, appended after it.
///
/// Written by a test rather than by the node's own writer, which is the only way to end a node
/// *while a supervisor is already reading the journal* — the supervisor's own view of what is
/// non-terminal is the records, so appending one is how a test says "it finished" to a process it
/// is not otherwise talking to.
fn a_node_exited(path: &Path, agent: &str, description: &str) {
    use marion_core::contract::{ExitStatus, ProcessExit};
    use marion_core::journal::{Exited, RecordKind};
    seed(
        path,
        3,
        RecordKind::Exited(Exited {
            agent_id: marion_core::contract::AgentId(agent.into()),
            status: ExitStatus::Ok,
            exit: ProcessExit {
                code: Some(0),
                signal: None,
                description: description.into(),
            },
        }),
    );
}

/// The same node, having exited: nothing in §5.7's exclusion list is left holding.
fn a_finished_node(path: &Path, agent: &str) {
    a_running_node(path, agent, 1);
    a_node_exited(path, agent, "the node finished");
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
///
/// **Two things this test used to be able to pass without.** First, the survival half passed
/// vacuously while `idle_exit_eligible` was gated on an explicit quit having found nothing:
/// `DetachAll` answered `Resident`, no exit was ever armed, and a supervisor that could not exit
/// under *any* circumstance satisfied *"refuses to exit"* perfectly. The second half below is the
/// control for that — the same process, the same journal, the node finishing — and it can only pass
/// if the timer was live and the node was the only thing holding it.
///
/// Second, its "running node" was three records naming pid 1. The pid is a real one now: what a
/// supervisor holds today **is** those records — it has neither the `Child` nor the channel, which
/// is §11 item 28 — so this test cannot claim more than the journal says, and it should not seed a
/// pid that makes the claim look bigger than it is.
#[test]
fn a_supervisor_holding_a_non_terminal_node_refuses_to_exit_until_that_node_finishes() {
    let bed = Bed::new("resident");
    let journal = marion_core::paths::ProjectDir::new(&bed.state, &bed.root).journal();
    // A process this test really started, so the journal names something that is actually running.
    let mut held = std::process::Command::new("sleep")
        .arg("120")
        .spawn()
        .expect("a real process to be non-terminal about");
    a_running_node(&journal, "root", held.id() as i32);

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

    // **The control.** The node ends, nobody connects, nobody quits, and the supervisor that had
    // been refusing for five graces goes on its own. Without this, "refuses to exit" is satisfied
    // by a supervisor that can never exit at all.
    let _ = held.kill();
    let _ = held.wait();
    a_node_exited(
        &journal,
        "root",
        "the node finished while nobody was attached",
    );
    assert!(
        until(|| !alive(id.pid)),
        "the only clause holding it cleared, so §5.7 permits the exit it had been declining"
    );
    assert!(
        journal_tags(&journal).contains(&"SupervisorExited".to_string()),
        "and it journaled the departure it did make: {:?}",
        journal_tags(&journal)
    );
    assert!(
        bed.supervisors().is_empty(),
        "nothing left for the fixture to kill"
    );
}

/// **NC — a linked worktree and its main repository are one project, for the socket and for the
/// journal alike.**
///
/// §2 states the rule once and it governs both: *"Both the supervisor and its state are keyed on
/// the **project root** (git common-dir, falling back to cwd) — not cwd, since worktree children
/// (§6.6) have different cwds and would otherwise hash to different supervisors."* Until this test
/// existed the two halves of that sentence were implemented differently — `socket::resolve` hashed
/// the common dir and `root::prepare` hashed the repo it was handed — so `marion run --repo <linked
/// worktree>` bound a supervisor at one key, journalled at a second, and any bridge or TUI calling
/// `resolve` in the same directory dialled a third. Three keys for one project, and the one that
/// answers is a supervisor tailing a journal nobody is writing.
///
/// Asserted through `root::prepare` rather than against `project_root` alone, because the identity
/// of the *function* was never the bug: both spellings were correct and they were called with
/// different arguments. Only the site that decides where a run's records land can show that.
/// §6.6's own case is the negative control below it — the worktree really is a different directory,
/// so an implementation that simply canonicalised would pass the first assertion and fail this one.
#[test]
fn a_linked_worktree_resolves_to_its_main_repositorys_supervisor_and_journal() {
    let bed = Bed::new("worktree-key");
    let main = bed.state.join("repo");
    std::fs::create_dir_all(&main).expect("repo dir");
    let git = |dir: &Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    git(&main, &["init", "-q", "-b", "main"]);
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "t"]);
    std::fs::write(main.join("a.txt"), b"a\n").expect("a file");
    git(&main, &["add", "."]);
    git(&main, &["commit", "-qm", "one"]);
    let wt = bed.state.join("linked");
    git(
        &main,
        &["worktree", "add", "-q", &wt.to_string_lossy(), "-b", "side"],
    );

    let key_main = marion_supervisor::socket::project_root(&main);
    let key_wt = marion_supervisor::socket::project_root(&wt);
    assert_eq!(
        key_wt, key_main,
        "§2's key is the git common dir, so a linked worktree resolves to its main repository's"
    );
    assert_ne!(
        wt.canonicalize().expect("the worktree exists"),
        key_wt,
        "and it is not merely the worktree canonicalised — §6.6's whole point is that the cwds \
         differ"
    );
    assert_eq!(
        socket_paths(&bed.state, &key_wt, unsafe { getuid() }).socket(),
        socket_paths(&bed.state, &key_main, unsafe { getuid() }).socket(),
        "one project, one socket"
    );

    // And the site that decides where a run's records land agrees with it.
    let spec = |repo: &Path| marion_supervisor::root::RootSpec {
        agent_type: "claude".into(),
        prompt: "unused: nothing is launched here".into(),
        repo: repo.to_path_buf(),
        state: bed.state.clone(),
        base_url: None,
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        model: None,
        no_change_record: true,
        auth: marion_harness::Auth::Canned,
    };
    let from_wt = marion_supervisor::root::prepare(&spec(&wt)).expect("a root prepares");
    let from_main = marion_supervisor::root::prepare(&spec(&main)).expect("a root prepares");
    assert_eq!(
        from_wt.project.journal(),
        from_main.project.journal(),
        "a run in a linked worktree writes the main repository's journal, which is the file the \
         one supervisor is tailing"
    );
    assert_eq!(
        from_wt.project.path(),
        marion_core::paths::ProjectDir::new(&bed.state, &key_main).path(),
        "and it is §2's key that decides, not the repo the run was pointed at"
    );
}

/// **NC — a supervisor whose last client vanished without quitting still leaves, if nothing is
/// left to supervise.**
///
/// §7.3.1 makes a dropped socket the crash case: *"nothing happens to agents"*. It says nothing
/// about the supervisor's own lifetime, and §5.7's answer for that is unconditional — *"with zero
/// clients and zero non-terminal nodes, the supervisor MAY exit after an idle grace period"*.
/// Nothing there requires a client to have said goodbye first, and requiring it means every
/// SIGKILLed TUI leaves a supervisor holding a project it has no work in — the leak the separate
/// process was meant to make visible rather than create.
///
/// The client here is dropped, never quit — the same thing a crashed one does — so this exercises
/// the crash path end to end, not a quit with the call omitted.
#[test]
fn a_client_that_vanished_without_quitting_leaves_an_empty_supervisor_free_to_go() {
    let bed = Bed::new("vanished");
    let journal = marion_core::paths::ProjectDir::new(&bed.state, &bed.root).journal();
    a_finished_node(&journal, "root");

    let ensured = ensure_supervisor(&bed.paths, &bed.launch()).expect("a supervisor starts");
    let id = published(&bed.paths);
    // No `session/quit`: the connection simply goes, exactly as a killed client's would.
    drop(ensured);

    assert!(
        until(|| !alive(id.pid)),
        "§5.7: zero clients and zero non-terminal nodes permits the exit, and a dropped socket is \
         not what withholds it"
    );
    assert!(
        journal_tags(&journal).contains(&"SupervisorExited".to_string()),
        "it left on §5.7's terms, so it says so: {:?}",
        journal_tags(&journal)
    );
    assert!(
        bed.supervisors().is_empty(),
        "and nothing of it is left for the fixture to kill"
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
/// **That question has now been answered in one direction and not the other**, which is what this
/// test's earlier revision asked whoever changed it to do. Failing closed is kept: a frozen tree may
/// name live work, and a supervisor that exited on a reading it knows is stale would be guessing.
/// What is no longer pinned is the *answer given to the operator*. Reporting
/// `Resident(NonTerminalNode)` sent them looking for a node — the tree is frozen, so that clause is
/// as stale as everything else in it — when the fact worth reporting is that marion stopped reading
/// the file. `Resident(RegistryStopped)` says that, and the supervisor's log carries the reason and
/// the offset (§7.4).
///
/// **Clearing the condition is still not implemented and is still not cheap** (§11 item 29):
/// `tests/run_stream.rs` reaps a supervisor stranded exactly this way and cites this test for why
/// it has to. Immortal is unchanged; *misleading* is what was fixed.
#[test]
fn a_journal_the_registry_cannot_parse_freezes_the_exit_predicate_and_says_so_by_name() {
    let bed = Bed::new("frozen");
    let journal = marion_core::paths::ProjectDir::new(&bed.state, &bed.root).journal();

    let ensured = ensure_supervisor(&bed.paths, &bed.launch()).expect("a supervisor starts");
    let id = published(&bed.paths);
    // **The node is journaled after the supervisor is up, which is the production order** — and
    // here it is load-bearing rather than incidental. A node already `Live` when a supervisor boots
    // is §7.2's restart case: `restart.rs` marks it `Orphaned`, and an orphan is not somebody's
    // agent, so `detached` would be empty and this test would be about a tree of lost nodes rather
    // than the frozen tail it is named for.
    a_running_node(&journal, "root", 1);
    // The supervisor has already folded the running node, so what follows is about the *tail* and
    // not about a supervisor that never read anything.
    assert!(
        until(|| matches!(
            session_quit(&bed.paths),
            marion_proto::QuitOutcome::Detached { ref detached, .. }
                if detached.as_slice() == [marion_core::contract::AgentId("root".into())]
        )),
        "the running node is folded and detachable before anything is corrupted"
    );

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
            marion_proto::ResidentReason::RegistryStopped
        ),
        "the exit record is on disk and the supervisor cannot see it — and the operator is told \
         that, rather than being sent after whichever clause the frozen prefix still satisfies"
    );
    drop(ensured);
    std::thread::sleep(GRACE * 3);
    assert!(
        alive(id.pid),
        "and no amount of waiting clears it, which is why the reap in run_stream.rs exists"
    );
    // The whole of what this test's own fixture is owed: the process it stranded is one nothing
    // else will end.
    assert_eq!(bed.supervisors(), [id.pid]);
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
    goes_on_its_own(&bed);
}
