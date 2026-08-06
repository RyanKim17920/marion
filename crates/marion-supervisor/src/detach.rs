//! How a supervisor detaches, and who starts one (design §5.7, §10; **S15**).
//!
//! `socket.rs` decides *where* the socket is and *who* may bind it. This decides what kind of
//! process does the binding, and how it comes to exist at all.
//!
//! # The mechanism is chosen and measured; this module only wires it
//!
//! `tests/fixtures/s15/README.md` closes §11 item 25 with a measurement, not an argument:
//! **double `fork` plus `setsid`**. Its identity table records what that makes the supervisor —
//! `ppid 1`, a **process group containing only itself**, sharing neither session nor group with its
//! launcher, and **not** a session leader. The two rejected alternatives are rejected on measured
//! grounds and are not re-opened here:
//!
//! * an ordinary spawn with the launcher exiting keeps *"the launcher's group and the launcher's
//!   session"*, and S15 measured the collateral that follows — a `killpg` on that group killed *"an
//!   innocent sibling job that merely shared the launcher's group"*, which under a real `marion`
//!   launched from a shell is the shell's job;
//! * a single `fork` + `setsid` is identical on every relation item 25 names, and loses on one
//!   measured tiebreak: a supervisor that **leads** its session *"acquires a controlling terminal
//!   the moment it opens a tty slave without `O_NOCTTY`"* (`ctty.json`: `setsid_leader` came back
//!   with `ttys029`, `setsid_double` with `??`). A controlling terminal is a channel through which a
//!   hangup reaches the process that detaching was meant to detach. marion opens no ttys today, and
//!   S11 exists, so *"the second fork costs one `fork` once"*.
//!
//! `launchd` was rejected on its own measurement — with the plain `KeepAlive` it restarted a
//! cleanly-exited payload roughly every ten seconds, which *"would fight §5.7's lifetime rules"* and
//! fill the journal with exit records describing a decision the system immediately overrode.
//!
//! # Three stages, named, because the invariant of each is checkable
//!
//! The double fork is spelled as three `marion-supervisor serve` invocations rather than as raw
//! `fork(2)` calls. That is not squeamishness about `unsafe`: between `fork` and `exec` only
//! async-signal-safe calls are legal, and a Rust process with threads, allocators and a panic
//! runtime is a place where that rule is easy to break silently. Three `exec`s make each stage a
//! process whose preconditions can be *asserted at its own entry* — and they are.
//!
//! | stage | argv | what it is | what it must be true of itself |
//! |---|---|---|---|
//! | 1 | `serve` | the entry point every caller takes | nothing; it is whatever its caller made it |
//! | 2 | `serve --session-leader` | the **middle** process S15's table names | it calls `setsid` and must end up leading its own session |
//! | 3 | `serve --detached` | the supervisor | it must **not** lead its session and must **not** lead its group |
//!
//! Stage 1 exists so that stage 2's `setsid` cannot fail. `setsid(2)` returns `EPERM` to a process
//! group leader, and a shell with job control makes every command it runs a group leader — so
//! `marion-supervisor serve` typed by hand would be one, while the same command spawned by `marion
//! run` would not. Rather than have the mechanism depend on who typed it, stage 1 spawns stage 2 and
//! stage 2 is a child of a `Command`, which never sets a process group, and therefore never a group
//! leader. **Every caller takes the same path**, which is what makes one measurement cover all of
//! them.
//!
//! Stage 3's entry check is the load-bearing one, and it **refuses** rather than degrades. A stage 3
//! that found itself leading a session would be an `inherit`- or `setsid_leader`-shaped supervisor,
//! which is to say one reachable by a terminal hangup; serving a fleet from it would trade the exact
//! property this module exists to obtain for the appearance of having obtained it. Refusing is
//! loud, and a supervisor that did not start is a supervisor a client will start again.
//!
//! # Why the supervisor is told its project rather than deriving it
//!
//! Stage 3 `chdir`s to `/`. A detached process holding a cwd pins a directory that may be deleted
//! out from under it — and, worse for marion specifically, `socket::project_root` reads `cwd` and
//! shells out to `git`. A supervisor deriving its own project from a cwd it inherited is a
//! supervisor whose socket path depends on which directory its launcher happened to be in, which is
//! the *one* thing §10 says must not vary (*"the socket path is identical in both, which is what
//! makes M2's split invisible to children"*). So the launcher resolves `(state, project root)` and
//! passes both; stage 3 recomputes the path from them with the same pure `socket_paths`, and the
//! agreement is structural rather than remembered.
//!
//! # What starting on demand means here
//!
//! §5.7: *"On demand, by the first client that dials the §2 socket path and finds nothing
//! listening … the start MUST be idempotent under contention — one supervisor wins, the loser dials
//! the winner rather than erroring or starting a second."*
//!
//! [`ensure_supervisor`] is the client half and it is deliberately **not** a second race. It dials;
//! if that succeeds nothing is spawned at all. If it does not, it spawns one stage 1 and then goes
//! back to dialing. The exclusion happens one process later, inside stage 3, in
//! [`socket::acquire`](crate::socket::acquire) — the flock the module next door already implements
//! and already tests under sixteen-way contention. A stage 3 that gets [`Acquired::Dialed`] learns
//! that somebody beat it, **drops the connection and exits 0 without journaling anything**, because
//! a process that never held the lock was never a supervisor and has no exit to record.
//!
//! So `N` racing clients may transiently create `N` stage-3 processes and exactly one survives. That
//! is the cross-process reading of §5.7's rule, and it costs one flock rather than a second
//! exclusion protocol that would have to agree with the first.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::socket::{SocketError, SocketPaths, SupervisorIdentity};

unsafe extern "C" {
    fn setsid() -> i32;
}

/// The subcommand. One word, on `marion-supervisor` and not on `marion`: §10's table already names
/// the detached process, and §5.7 starts it *on demand by a client* — so there is no operator who
/// needs to type it, and `bin/marion.rs`'s refusal of every argv[0] but `run` stays where it is.
pub const SERVE: &str = "serve";

/// Stage 2's marker. See the module doc's table.
pub const SESSION_LEADER_FLAG: &str = "--session-leader";

/// Stage 3's marker.
pub const DETACHED_FLAG: &str = "--detached";

/// How long [`ensure_supervisor`] keeps dialing before reporting that it could not reach one.
///
/// **A bound, not a measurement.** It covers two `fork`+`exec` pairs and one `bind`, which is
/// milliseconds, and it is generous enough that a loaded machine never decides the answer. Nothing
/// asserts on how long the call takes; the tests assert on *how many* supervisors ended up serving.
pub const READY_DEADLINE: Duration = Duration::from_secs(10);

/// How often [`ensure_supervisor`] re-dials while waiting.
const DIAL_POLL: Duration = Duration::from_millis(5);

/// Everything stage 3 needs to be told, because it will not be in a position to look any of it up.
#[derive(Debug, Clone)]
pub struct Launch {
    /// The `marion-supervisor` binary. Resolved by the launcher — a detached process cannot be
    /// relied on to find it on `$PATH`, since it does not inherit an interactive shell's.
    pub program: PathBuf,
    /// `<state>`, already resolved by §4.3's precedence.
    pub state_dir: PathBuf,
    /// The **canonical project root** §2 keys on, already resolved by [`crate::socket::project_root`].
    pub project_root: PathBuf,
    /// §5.7's idle grace. Passed rather than defaulted so a test can assert ordering without
    /// waiting out five minutes, and so the default lives in exactly one place
    /// ([`crate::serve::DEFAULT_IDLE_GRACE`]).
    pub idle_grace: Duration,
}

impl Launch {
    /// This project's socket, lock, identity and log — from `(state, project root)` and §2's rule,
    /// which is the *only* derivation any stage performs. See the module doc on why stage 3 is told
    /// its project rather than deriving it from a cwd.
    pub fn paths(&self) -> SocketPaths {
        crate::socket::socket_paths(
            &self.state_dir,
            &self.project_root,
            // SAFETY: reads the calling process's real uid and cannot fail.
            unsafe { uid() },
        )
    }

    /// The argv stage 1 is started with. Stages 2 and 3 append their own marker to it, so the three
    /// can never disagree about the project they are serving.
    pub fn argv(&self) -> Vec<String> {
        vec![
            SERVE.to_string(),
            "--state-dir".to_string(),
            self.state_dir.display().to_string(),
            "--project-root".to_string(),
            self.project_root.display().to_string(),
            "--idle-grace-ms".to_string(),
            self.idle_grace.as_millis().to_string(),
        ]
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DetachError {
    #[error("marion could not start a supervisor ({program}): {source}")]
    Spawn {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "marion started a supervisor for this project but could not reach {path} within \
         {waited_ms} ms. Nothing was assumed about whether one is running: the socket was dialed \
         and did not answer, and marion did not unlink it, because only the supervisor lock proves \
         a socket is dead (§5.7)."
    )]
    NotReachable { path: PathBuf, waited_ms: u128 },
    #[error(transparent)]
    Socket(#[from] SocketError),
}

/// What [`ensure_supervisor`] found or made.
#[derive(Debug)]
pub struct Ensured {
    /// A live connection to the supervisor. Held by the caller, because a client that asked for a
    /// supervisor and then threw the connection away would have proved nothing: §5.7's outcome is
    /// *"the loser dials the winner"*, and this **is** the dial.
    pub stream: std::os::unix::net::UnixStream,
    /// Whether this call spawned anything. `false` means one was already serving — the ordinary
    /// case for every client after the first, and the property a second `marion run` must have.
    pub started: bool,
}

/// §5.7's start: dial, and start one **only** if nothing answers.
pub fn ensure_supervisor(paths: &SocketPaths, launch: &Launch) -> Result<Ensured, DetachError> {
    ensure_supervisor_within(paths, launch, READY_DEADLINE)
}

/// [`ensure_supervisor`] with the bound named, so a test can observe the *failure* without waiting
/// out a bound written for a machine under load. The bound never decides a verdict: a supervisor
/// that is serving answers the first dial, and one that never binds is unreachable at any bound.
pub fn ensure_supervisor_within(
    paths: &SocketPaths,
    launch: &Launch,
    within: Duration,
) -> Result<Ensured, DetachError> {
    let deadline = Instant::now() + within;
    let mut started = false;
    let mut attempts = 0usize;
    let mut last_attempt = Instant::now();
    let mut last_spawn: Option<DetachError> = None;
    loop {
        match std::os::unix::net::UnixStream::connect(paths.socket()) {
            Ok(stream) => return Ok(Ensured { stream, started }),
            Err(e) if nobody_answered(&e) => {}
            Err(e) => {
                return Err(DetachError::Socket(SocketError::Dial {
                    path: paths.socket().to_path_buf(),
                    source: e,
                }));
            }
        }
        if attempts < MAX_START_ATTEMPTS
            && (attempts == 0
                || (last_attempt.elapsed() >= RETRY_QUIET
                    && crate::socket::nobody_is_serving(paths)))
        {
            attempts += 1;
            last_attempt = Instant::now();
            started = true;
            // **A failed launcher stage is remembered, not returned.** Three of this start's kill
            // windows leave a stage reporting failure *after* the supervisor is already on its way:
            // kill stage 1 once it has spawned stage 2, or stage 2 once it has spawned stage 3, and
            // the `wait` inside comes back non-zero over a socket that is about to answer.
            // Returning that status would tell the operator nothing started while leaving a
            // supervisor running to contradict them — and nothing outside can settle which report
            // was true, because the *next* invocation simply finds the socket answering. The dial
            // is the authority on whether a supervisor exists; an exit status is evidence about how
            // one attempt went, and it is reported at the end only if no supervisor ever appeared.
            if let Err(e) = spawn_stage_one(launch) {
                last_spawn = Some(e);
            }
            continue;
        }
        if Instant::now() >= deadline {
            return Err(last_spawn.unwrap_or(DetachError::NotReachable {
                path: paths.socket().to_path_buf(),
                waited_ms: within.as_millis(),
            }));
        }
        std::thread::sleep(DIAL_POLL);
    }
}

/// How many supervisors one call will try to bring into existence before it only waits.
///
/// **Not "once", and not "on every failed dial".** Once was wrong in the direction the review
/// found: a stage 3 that dies before it binds — a failed `exec`, an OOM kill, a stage-2 refusal —
/// leaves `started` true, nothing listening and the lock free, so the client sits out its whole
/// bound to report [`DetachError::NotReachable`] about a project it could have started a supervisor
/// in at any moment. Re-spawning on every failed dial is the failure the original comment named and
/// is still refused: it aims a fork bomb at the project whose supervisor is two syscalls from
/// binding.
///
/// So a retry needs *evidence*, and it uses the evidence this system already trusts for exactly
/// this question — [`crate::socket::nobody_is_serving`], the same `flock` that decides who serves.
/// A lock that can be taken is proof that no supervisor holds it, which is precisely what a
/// stillborn stage 3 leaves behind. The cap covers the one case the lock cannot see: a stage 3 that
/// has `exec`ed but not yet reached its `flock` is indistinguishable from one that never will, so a
/// few extra stage 3s may be created — which §5.7's design already absorbs, *"N racing clients may
/// transiently create N stage-3 processes and exactly one survives"*. Three is where a bound stops
/// being a retry and starts being a loop.
const MAX_START_ATTEMPTS: usize = 3;

/// How long an attempt is left alone before a free lock is read as *"it is not coming"*.
///
/// A free lock is proof that nobody is serving **now**, and during an ordinary start it is true for
/// the whole time the three stages are `exec`ing — so retrying on that evidence alone starts a
/// second chain during every normal start. That is not a correctness failure, since `socket::acquire`
/// makes the redundant stage 3 stand down, but it is work nobody asked for, and it was measured
/// here: without this interval a mutation that broke stage 2's log redirect still passed, because
/// the *second* chain found the directory the first one's supervisor had created.
///
/// So the lock's evidence is combined with the one thing that separates "starting" from "stillborn":
/// how long it has been that way. 250 ms is four orders of magnitude above the syscalls it covers
/// and far below anything an operator perceives, and it decides no verdict — a supervisor that binds
/// is dialed on the next poll whatever this is, and one that never binds is unreachable at any
/// value.
const RETRY_QUIET: Duration = Duration::from_millis(250);

/// The same three readings `socket::acquire` treats as "nobody answered", and for the same reason:
/// none of them is conclusive on its own, which is why the lock and not the dial decides.
fn nobody_answered(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ConnectionRefused | ErrorKind::NotFound | ErrorKind::WouldBlock
    )
}

/// Start stage 1 and **wait for it**, so nothing is left unreaped in the launcher.
///
/// Waiting costs one process lifetime — stage 1 exits as soon as stage 2 does, and stage 2 exits as
/// soon as it has spawned stage 3 — and buys the property that `marion run` never accumulates
/// zombies. It does *not* wait for the supervisor: stage 3 outlives all of this by construction,
/// which is the point, and readiness is established by dialing rather than by a parent's `wait`.
fn spawn_stage_one(launch: &Launch) -> Result<(), DetachError> {
    let status = Command::new(&launch.program)
        .args(launch.argv())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // stderr is inherited on stages 1 and 2 **only**: a supervisor that could not start must
        // say so where the operator who asked for it is looking. Stage 3 redirects it to a file
        // before it becomes long-lived — see [`spawn_stage_three`].
        .status()
        .map_err(|source| DetachError::Spawn {
            program: launch.program.clone(),
            source,
        })?;
    if status.success() {
        return Ok(());
    }
    Err(DetachError::Spawn {
        program: launch.program.clone(),
        source: std::io::Error::other(format!(
            "the supervisor's launcher stage exited with {status}"
        )),
    })
}

/// Stage 1: spawn stage 2 and wait. See the module doc for why this hop exists at all.
pub fn run_stage_one(launch: &Launch) -> Result<(), DetachError> {
    let mut argv = launch.argv();
    argv.push(SESSION_LEADER_FLAG.to_string());
    let status = Command::new(&launch.program)
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .map_err(|source| DetachError::Spawn {
            program: launch.program.clone(),
            source,
        })?;
    if status.success() {
        return Ok(());
    }
    Err(DetachError::Spawn {
        program: launch.program.clone(),
        source: std::io::Error::other(format!(
            "the supervisor's middle stage exited with {status}"
        )),
    })
}

/// Stage 2 — S15's **middle** process: become a session leader, fork once more, and die.
///
/// Its death is what leaves stage 3 with a `sid` and a `pgid` that name a process which no longer
/// exists (S15's identity table, `setsid_double`), and that is the stated price of the mechanism,
/// paid deliberately. It is also why nothing may ever `killpg` a marion supervisor by its pid — see
/// [`SupervisorIdentity`].
pub fn run_stage_two(launch: &Launch) -> Result<(), DetachError> {
    // SAFETY: `setsid` takes no arguments and touches nothing this process owns.
    if unsafe { setsid() } == -1 {
        let source = std::io::Error::last_os_error();
        return Err(DetachError::Spawn {
            program: launch.program.clone(),
            source: std::io::Error::other(format!(
                "the supervisor's middle stage could not create a session ({source}). setsid(2) \
                 refuses a process group leader, and stage 1 exists precisely so that this process \
                 is not one -- so reaching this means `{SERVE} {SESSION_LEADER_FLAG}` was started \
                 by something other than stage 1. marion refuses to continue rather than serve a \
                 fleet from a supervisor that kept its launcher's session (S15)."
            )),
        });
    }
    debug_assert_eq!(
        SupervisorIdentity::own().sid,
        SupervisorIdentity::own().pid,
        "setsid returned success, so this process leads its own session"
    );
    spawn_stage_three(launch)
}

/// Stage 2's fork: the supervisor itself, detached and not waited for.
fn spawn_stage_three(launch: &Launch) -> Result<(), DetachError> {
    let mut argv = launch.argv();
    argv.push(DETACHED_FLAG.to_string());
    let paths = launch.paths();
    // **The directory first.** Stage 3 creates it — inside `socket::acquire`, several syscalls after
    // this — so on a project nothing has ever served, opening the log here found no directory,
    // degraded to `/dev/null`, and left the one process nobody is watching unable to report
    // anything. Every stage-3 failure on a *fresh* project was invisible for that reason, which is
    // also why the failures around it were hard to see. Sharing `socket::prepare_dir` rather than
    // creating the directory here keeps the `/tmp` audit in one place.
    let _ = crate::socket::prepare_dir(&paths);
    let log = paths.log().to_path_buf();
    // Best-effort *after* that: a supervisor whose log could not be opened still serves, and stderr
    // goes to `/dev/null` rather than to a terminal this process is about to stop having.
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    Command::new(&launch.program)
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        // `/` and not the launcher's cwd: see the module doc. A detached process holding a cwd pins
        // a directory that may be deleted, and marion's own path derivation reads `cwd`.
        .current_dir(Path::new("/"))
        .spawn()
        .map(|_child| ())
        .map_err(|source| DetachError::Spawn {
            program: launch.program.clone(),
            source,
        })
}

/// Where stage 3's stderr goes: [`SocketPaths::log`](crate::socket::SocketPaths::log), derived from
/// what this launch was told and nothing else.
///
/// It used to be `dir().join("supervisor.log")`, which is the project's own directory under
/// `<state>` and the **shared** `/tmp/marion-<uid>` under §2's fallback — one log for every
/// overflowing project on the machine, interleaved. The path now comes from the same pure function
/// as the socket, so the two cannot disagree about which project they belong to.
pub fn log_path(launch: &Launch) -> PathBuf {
    launch.paths().log().to_path_buf()
}

unsafe extern "C" {
    #[link_name = "getuid"]
    fn uid() -> u32;
}

/// Which of the three stages an argv names. See the module doc's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    One,
    SessionLeader,
    Detached,
}

/// Parse `serve`'s argv into the stage and the project it was told about.
///
/// **An unknown flag is a refusal, never a silent ignore** — `bin/marion.rs`'s rule, and it matters
/// more here: this argv crosses two process boundaries, so a flag dropped in stage 1 would produce a
/// stage 3 serving a *different* project than the client that asked for it dialed, with nothing
/// anywhere reporting a problem.
pub fn parse_serve(program: PathBuf, argv: &[String]) -> Option<(Launch, Stage)> {
    let mut state_dir: Option<PathBuf> = None;
    let mut project_root: Option<PathBuf> = None;
    let mut idle_grace: Option<Duration> = None;
    let mut stage = Stage::One;
    let mut rest = argv.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--state-dir" => state_dir = Some(PathBuf::from(rest.next()?)),
            "--project-root" => project_root = Some(PathBuf::from(rest.next()?)),
            "--idle-grace-ms" => {
                idle_grace = Some(Duration::from_millis(rest.next()?.parse().ok()?))
            }
            SESSION_LEADER_FLAG => stage = Stage::SessionLeader,
            DETACHED_FLAG => stage = Stage::Detached,
            _ => return None,
        }
    }
    Some((
        Launch {
            program,
            state_dir: state_dir?,
            project_root: project_root?,
            idle_grace: idle_grace.unwrap_or(crate::serve::DEFAULT_IDLE_GRACE),
        },
        stage,
    ))
}

/// `marion-supervisor serve …`, whichever stage it is.
pub fn run_serve(program: PathBuf, argv: &[String]) -> Result<(), DetachError> {
    let Some((launch, stage)) = parse_serve(program.clone(), argv) else {
        return Err(DetachError::Spawn {
            program,
            source: std::io::Error::other(format!(
                "usage: marion-supervisor {SERVE} --state-dir <dir> --project-root <dir> \
                 [--idle-grace-ms <n>]. A supervisor is started on demand by the first client that \
                 dials and finds nothing listening (§5.7); it is not normally typed."
            )),
        });
    };
    match stage {
        Stage::One => run_stage_one(&launch),
        Stage::SessionLeader => run_stage_two(&launch),
        Stage::Detached => run_stage_three(&launch),
    }
}

/// Stage 3 — the supervisor. Check the identity, take the socket or stand down, and serve.
///
/// **Standing down is silent and journals nothing.** A stage 3 that loses `socket::acquire`'s race
/// gets [`Acquired::Dialed`](crate::socket::Acquired::Dialed), which means another supervisor holds
/// the lock — so this process never was one, has no clients, no nodes and no exit to record, and
/// §5.7's *"exit MUST be journaled"* does not apply to it. Writing a `SupervisorExited` here would
/// put a second supervisor's departure in a journal whose supervisor is still running.
pub fn run_stage_three(launch: &Launch) -> Result<(), DetachError> {
    use crate::handler::RegistryHandle;
    use crate::registry::{LiveRegistry, Registry};
    use crate::serve::Server;
    use crate::socket::{Acquired, acquire, socket_paths};
    use marion_core::paths::ProjectDir;

    ensure_detached(SupervisorIdentity::own()).map_err(|source| DetachError::Spawn {
        program: launch.program.clone(),
        source: std::io::Error::other(source.to_string()),
    })?;
    // SAFETY: reads the calling process's real uid and cannot fail.
    let paths = socket_paths(&launch.state_dir, &launch.project_root, unsafe { uid() });
    let Acquired::Serving(serving) = acquire(&paths)? else {
        return Ok(());
    };
    let project = ProjectDir::new(&launch.state_dir, &launch.project_root);
    let registry = Registry::boot(&project).map_err(|source| DetachError::Spawn {
        program: launch.program.clone(),
        source: std::io::Error::other(format!(
            "the supervisor could not open this project's journal: {source}"
        )),
    })?;
    let live = std::sync::Arc::new(LiveRegistry::follow(registry, REGISTRY_POLL));
    let sentry = serving.sentry();
    let server =
        Server::start_with_idle_grace(serving, RegistryHandle::new(live), launch.idle_grace);
    watch_entitlement(sentry);
    // §5.7: the supervisor's lifetime is not its client's. The only way out of this call is the
    // accept loop's own idle exit, which has already journaled the record by the time it returns.
    server.wait();
    Ok(())
}

/// **Stand down if this process stops being the supervisor of the project it is serving.**
///
/// `socket::Serving::still_entitled` explains what can stop being true and why nothing can prevent
/// it: remove `<state>/<hash>` under a running supervisor and the `flock` that makes one supervisor
/// per project keeps excluding an inode nobody can name any more, while a new caller takes a fresh
/// lock at the same pathname and serves. Exclusion lives in a name both processes can reach, and the
/// name is what was removed.
///
/// What is left is a choice about the evicted process, and every alternative to leaving is worse.
/// It cannot go on serving: its socket is unlinked, so no client will ever reach it again, and its
/// project directory is gone, so §5.7's *"the exit MUST be journaled"* has nowhere to write — it
/// would be immortal, holding whatever its nodes hold, invisible to every subsequent invocation.
/// It cannot journal an exit it cannot write. So it says why, on the stderr the launcher pointed at
/// this project's log, and goes.
///
/// `std::process::exit` and not a graceful stop, deliberately. A graceful stop would try to write
/// the exit record §5.7 requires, into the journal that is no longer there; the honest thing is to
/// leave *without* one, because "it died" is exactly what a later reader should conclude about a
/// supervisor whose project was deleted underneath it. The non-zero status says the same to
/// whoever is watching the process.
fn watch_entitlement(sentry: Option<crate::socket::Sentry>) {
    let Some(sentry) = sentry else {
        // No sentry means the lock descriptor could not be duplicated. Watching nothing is the
        // right failure: a supervisor that cannot check its entitlement has not lost it.
        return;
    };
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(ENTITLEMENT_POLL);
            if sentry.still_entitled() {
                continue;
            }
            eprintln!("marion-supervisor: standing down -- {}", sentry.why());
            std::process::exit(70);
        }
    });
}

/// How often the supervisor re-checks that it is still the supervisor.
///
/// Two `stat`s a second, which is not a measurement of anything and does not need to be: the event
/// it watches for is an operator removing a directory, and the cost of noticing a second later is a
/// second of a split brain that has already happened. Polling is the only shape available —
/// `<state>/<hash>` going away is not an event any descriptor here delivers.
const ENTITLEMENT_POLL: Duration = Duration::from_millis(500);

/// How often the detached supervisor folds new journal bytes.
///
/// `journal.rs` group-commits on a ~50 ms timer, so a record becomes visible at that granularity and
/// polling faster buys latency that does not exist. Ten milliseconds keeps the supervisor's view
/// within one commit interval of the file without reading it in a spin — and every decision point
/// (`session/quit`, the idle-exit predicate) calls `LiveRegistry::refresh` synchronously anyway,
/// precisely so that no correctness claim rests on this number.
const REGISTRY_POLL: Duration = Duration::from_millis(10);

/// Why stage 3 refused to serve. Both arms mean the double fork did not happen.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NotDetached {
    #[error(
        "this process leads its own session (sid {sid} == pid {pid}), so it is not the far side of \
         a double fork. A session leader with no controlling terminal takes ownership of the first \
         tty it opens without O_NOCTTY, and a controlling terminal is a channel through which a \
         hangup reaches the process that detaching was meant to detach (S15, ctty.json). marion \
         refuses to serve a fleet from here."
    )]
    SessionLeader { pid: i32, sid: i32 },
    #[error(
        "this process leads its own process group (pgid {pgid} == pid {pid}), so its group is not \
         the singleton group S15 measured a detached supervisor to be alone in. marion refuses to \
         serve a fleet from here."
    )]
    GroupLeader { pid: i32, pgid: i32 },
}

/// **Stage 3's entry check, asserted against S15's measurements rather than against a description
/// of them.**
///
/// The `setsid_double` row of the identity table says the supervisor is a session leader: **no**,
/// and a group leader: **no**. Both are checked, because they fail apart: a single `fork` + `setsid`
/// (S15's `setsid_leader`) leads *both*, and an ordinary spawn (`inherit`) leads *neither* while
/// still sharing the launcher's session — which is why [`ensure_detached`] is not, on its own,
/// sufficient evidence of the mechanism, and why the negative controls also compare against the
/// launcher's own identity.
pub fn ensure_detached(id: SupervisorIdentity) -> Result<(), NotDetached> {
    if id.leads_its_session() {
        return Err(NotDetached::SessionLeader {
            pid: id.pid,
            sid: id.sid,
        });
    }
    if id.leads_its_group() {
        return Err(NotDetached::GroupLeader {
            pid: id.pid,
            pgid: id.pgid,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three stages carry **one** description of the project between them, so stage 3 cannot
    /// serve a different socket than the client that asked for it dialed.
    #[test]
    fn every_stage_is_told_the_same_project_and_only_the_marker_differs() {
        let launch = Launch {
            program: PathBuf::from("/bin/marion-supervisor"),
            state_dir: PathBuf::from("/s"),
            project_root: PathBuf::from("/p/.git"),
            idle_grace: Duration::from_millis(250),
        };
        let base = launch.argv();
        assert_eq!(base[0], SERVE);
        assert!(base.contains(&"/s".to_string()));
        assert!(base.contains(&"/p/.git".to_string()));
        assert!(
            base.contains(&"250".to_string()),
            "the grace crosses the process boundary as milliseconds: {base:?}"
        );
        assert!(
            !base.contains(&SESSION_LEADER_FLAG.to_string())
                && !base.contains(&DETACHED_FLAG.to_string()),
            "stage 1 carries no marker, so a bare `serve` is unambiguously the entry point"
        );
    }

    /// **NC — the two rejected mechanisms are told apart, and they are told apart differently.**
    ///
    /// S15 measured three identities. This asserts that stage 3's check accepts exactly one of
    /// them, and — the half that matters — that the two refusals are **distinguishable**, because a
    /// check that refused `setsid_leader` and `inherit` for the same stated reason would be a check
    /// that had not actually looked at the property the tiebreak turns on.
    ///
    /// The numbers are S15's shapes, not S15's literal pids: `setsid_leader` leads both, the chosen
    /// `setsid_double` leads neither, and `inherit` leads neither *either* — which is the finding
    /// that makes this function insufficient on its own and is why the integration negative control
    /// also compares the supervisor against its launcher.
    #[test]
    fn stage_three_accepts_only_the_measured_double_fork_identity() {
        let leader = SupervisorIdentity {
            pid: 100,
            pgid: 100,
            sid: 100,
        };
        assert_eq!(
            ensure_detached(leader),
            Err(NotDetached::SessionLeader { pid: 100, sid: 100 }),
            "S15's setsid_leader row: session leader — the ctty hazard"
        );

        let group_leader_only = SupervisorIdentity {
            pid: 100,
            pgid: 100,
            sid: 7,
        };
        assert_eq!(
            ensure_detached(group_leader_only),
            Err(NotDetached::GroupLeader {
                pid: 100,
                pgid: 100
            }),
            "a different refusal, because it is a different failure"
        );

        let double = SupervisorIdentity {
            pid: 100,
            pgid: 99,
            sid: 99,
        };
        assert_eq!(
            ensure_detached(double),
            Ok(()),
            "S15's setsid_double row: pgid and sid name the middle process, which has exited"
        );

        // And the `inherit` row passes this check while being the mechanism S15 rejected, which is
        // stated here so nobody reads a green stage-3 check as proof of the mechanism.
        let inherit = SupervisorIdentity {
            pid: 100,
            pgid: 42,
            sid: 42,
        };
        assert_eq!(ensure_detached(inherit), Ok(()));
    }

    /// The refusals name the numbers, because an operator reading one has no other way to see what
    /// the process actually was.
    #[test]
    fn a_refusal_quotes_the_identity_it_refused() {
        let e = ensure_detached(SupervisorIdentity {
            pid: 100,
            pgid: 100,
            sid: 100,
        })
        .unwrap_err();
        let text = e.to_string();
        assert!(text.contains("100"), "{text}");
        assert!(
            text.contains("O_NOCTTY"),
            "the reason, not just the fact: {text}"
        );
    }
}
