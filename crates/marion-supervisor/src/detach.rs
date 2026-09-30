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
//! if that succeeds nothing is spawned at all. If it does not, it spawns one stage 1 and blocks on
//! the chain's **ready pipe** until stage 3 says a supervisor is serving or the chain has ended,
//! then dials again. The exclusion happens one process later, inside stage 3, in
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
    fn dup2(from: i32, to: i32) -> i32;
}

/// The subcommand. One word, on `marion-supervisor` and not on `marion`: §10's table already names
/// the detached process, and §5.7 starts it *on demand by a client* — so there is no operator who
/// needs to type it, and `bin/marion.rs`'s refusal of every argv[0] but `run` stays where it is.
pub const SERVE: &str = "serve";

/// Stage 2's marker. See the module doc's table.
pub const SESSION_LEADER_FLAG: &str = "--session-leader";

/// Stage 3's marker.
pub const DETACHED_FLAG: &str = "--detached";

/// The client's marker, carried by every stage: stdout is the client's **ready pipe**, and stage 3
/// writes one byte to it once a supervisor is serving. Without it every stage's stdout is
/// `/dev/null`, so a stage started by hand never writes into a stream it was not given for this.
pub const READY_FLAG: &str = "--ready-on-stdout";

/// [`Launch::auth`]'s flag. **Mandatory** — see that field.
pub const AUTH_FLAG: &str = "--auth";

/// [`Launch::base_url`]'s flag. Present exactly when the endpoint exists, which under
/// [`marion_harness::Auth::Canned`] is always and under `Inherited` is never.
pub const BASE_URL_FLAG: &str = "--base-url";

/// How long [`ensure_supervisor`] waits on the supervisors it starts before reporting that it could
/// not reach one.
///
/// **A bound, not a measurement.** It covers two `fork`+`exec` pairs and one `bind`, which is
/// milliseconds, and it is generous enough that a loaded machine never decides the answer. Nothing
/// asserts on how long the call takes; the tests assert on *how many* supervisors ended up serving.
pub const READY_DEADLINE: Duration = Duration::from_secs(10);

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
    /// Whether the nodes this supervisor spawns present a credential marion minted or the
    /// operator's own login — [`crate::run::Env::auth`].
    ///
    /// **Carried, never re-read from the environment.** Stage 3 could call the bridge's
    /// `auth_from_env`, and that function's documented rule is that an *absent* `MARION_AUTH`
    /// means [`marion_harness::Auth::Canned`] — correct where it lives, because every declaration
    /// written before the key existed meant canned. Here it would mean something else entirely: a
    /// detached supervisor started by a launcher whose environment did not survive the double fork
    /// would quietly run an operator's live fleet against a canned endpoint that is not listening,
    /// and report nothing. So the launcher states it and [`parse_serve`] refuses an argv that does
    /// not.
    ///
    /// **argv is safe for this.** The value is a two-valued enum with no secret in it
    /// ([`marion_harness::Auth::as_wire`]), and the credential paired with the canned endpoint is
    /// the literal [`crate::run::PLACEHOLDER_API_KEY`], documented there as not a secret and
    /// checked by nothing. Nothing here is world-visible in `ps` that was not already.
    pub auth: marion_harness::Auth,
    /// The canned provider's endpoint — [`crate::run::Env::base_url`].
    ///
    /// `None` is meaningful and is not "unstated": under [`marion_harness::Auth::Inherited`] marion
    /// overlays no endpoint at all and each harness resolves its own. So the two fields are
    /// validated as a pair — see [`parse_serve`] — rather than this one defaulting.
    pub base_url: Option<String>,
}

impl Launch {
    /// This project's socket, lock, identity and log — from `(state, project root)` and §2's rule,
    /// which is the *only* derivation any stage performs. See the module doc on why stage 3 is told
    /// its project rather than deriving it from a cwd.
    pub fn paths(&self) -> SocketPaths {
        crate::socket::socket_paths(
            &self.state_dir,
            &self.project_root,
            crate::socket::own_uid(),
        )
    }

    /// The argv stage 1 is started with. Stages 2 and 3 append their own marker to it, so the three
    /// can never disagree about the project they are serving.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = vec![
            SERVE.to_string(),
            "--state-dir".to_string(),
            self.state_dir.display().to_string(),
            "--project-root".to_string(),
            self.project_root.display().to_string(),
            "--idle-grace-ms".to_string(),
            self.idle_grace.as_millis().to_string(),
            // Always present, so its absence downstream is a fault and never a default. See the
            // field's doc for why an environment variable would have been the silent option.
            AUTH_FLAG.to_string(),
            self.auth.as_wire().to_string(),
        ];
        if let Some(url) = &self.base_url {
            argv.push(BASE_URL_FLAG.to_string());
            argv.push(url.clone());
        }
        argv
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
         a socket is dead."
    )]
    NotReachable { path: PathBuf, waited_ms: u128 },
    /// The supervisor answered and would not take this client's `session/hello`.
    #[error("{0}")]
    Hello(String),
    #[error(transparent)]
    Socket(#[from] SocketError),
}

/// What [`ensure_supervisor`] found or made.
#[derive(Debug)]
pub struct Ensured {
    /// A live connection to the supervisor, already past its `session/hello`. Held by the caller, because a client that asked for a
    /// supervisor and then threw the connection away would have proved nothing: §5.7's outcome is
    /// *"the loser dials the winner"*, and this **is** the dial.
    pub stream: std::os::unix::net::UnixStream,
    /// Whether this call spawned anything. `false` means one was already serving — the ordinary
    /// case for every client after the first, and the property a second `marion run` must have.
    pub started: bool,
}

/// §5.7's start: dial, and start one **only** if nothing answers.
///
/// Each start blocks on its chain's ready pipe ([`start_and_wait`]) rather than re-dialing on a
/// timer: one byte means a supervisor is serving, and the pipe's end means the chain is over
/// without one. Either way the dial that follows is the authority, and another start is made only
/// on the lock's proof that nobody holds it.
pub fn ensure_supervisor(paths: &SocketPaths, launch: &Launch) -> Result<Ensured, DetachError> {
    crate::socket::check_fallback_dir(paths)?;
    if let Some(stream) = dial_supervisor(paths)? {
        return Ok(Ensured {
            stream,
            started: false,
        });
    }
    let started = Instant::now();
    let deadline = started + READY_DEADLINE;
    let mut last_spawn: Option<DetachError> = None;
    for _ in 0..MAX_START_ATTEMPTS {
        // **A failed launcher stage is remembered, not returned.** Three of this start's kill
        // windows leave a stage reporting failure *after* the supervisor is already on its way:
        // kill stage 1 once it has spawned stage 2, or stage 2 once it has spawned stage 3, and
        // the `wait` inside comes back non-zero over a socket that is about to answer. Returning
        // that status would tell the operator nothing started while leaving a supervisor running
        // to contradict them — and nothing outside can settle which report was true, because the
        // *next* invocation simply finds the socket answering. The dial is the authority on
        // whether a supervisor exists; an exit status is evidence about how one attempt went, and
        // it is reported at the end only if no supervisor ever appeared.
        if let Err(e) = start_and_wait(launch, deadline) {
            last_spawn = Some(e);
        }
        if let Some(stream) = dial_supervisor(paths)? {
            return Ok(Ensured {
                stream,
                started: true,
            });
        }
        if Instant::now() >= deadline || !crate::socket::nobody_is_serving(paths) {
            break;
        }
    }
    Err(last_spawn.unwrap_or(DetachError::NotReachable {
        path: paths.socket().to_path_buf(),
        waited_ms: started.elapsed().as_millis(),
    }))
}

/// **A live supervisor's connection, past its `session/hello`** — or `None` while none answers.
fn dial_supervisor(
    paths: &SocketPaths,
) -> Result<Option<std::os::unix::net::UnixStream>, DetachError> {
    let Some(mut stream) = dial_live_supervisor(paths)? else {
        return Ok(None);
    };
    let who = crate::client_auth::identity_for(paths).map_err(DetachError::Hello)?;
    match crate::client_auth::hello(&mut stream, &who) {
        Ok(()) => Ok(Some(stream)),
        // The lock is held but the socket is still an older listener nobody answers on — a
        // supervisor starting now has not yet bound its own.
        Err(crate::client_auth::HelloError::Unanswered(_)) => Ok(None),
        Err(e) => Err(DetachError::Hello(e.to_string())),
    }
}

/// A stream to a supervisor that actually exists, or `None` when none does yet.
///
/// **A connection is not proof of a supervisor.** `socket.rs` measured on darwin 25.5.0 that
/// closing a descriptor is not synchronous with another descriptor's view of it: for a window after
/// a listener's process is gone, `connect` to its path still *succeeds*. A client that took that as
/// an answer would hand its operator a stream to a supervisor that does not exist — no
/// notifications, no responses, and a `marion run` that reports it attached to a fleet nobody is
/// enforcing. The lock is the proof this module already uses everywhere else, and a bound socket
/// implies a held lock by construction, so a successful dial over a *free* lock is a corpse and is
/// dialed again rather than returned.
fn dial_live_supervisor(
    paths: &SocketPaths,
) -> Result<Option<std::os::unix::net::UnixStream>, DetachError> {
    match std::os::unix::net::UnixStream::connect(paths.socket()) {
        Ok(stream) if !crate::socket::nobody_is_serving(paths) => Ok(Some(stream)),
        Ok(_corpse) => Ok(None),
        Err(e) if nobody_answered(&e) => Ok(None),
        Err(e) => Err(DetachError::Socket(SocketError::Dial {
            path: paths.socket().to_path_buf(),
            source: e,
        })),
    }
}

/// How many supervisors one call will try to bring into existence.
///
/// **Not "once", and not "on every failed dial".** Once was wrong in the direction the review
/// found: a stage 3 that dies before it binds — a failed `exec`, an OOM kill, a stage-2 refusal —
/// leaves nothing listening and the lock free, and a client that only waited would report
/// [`DetachError::NotReachable`] about a project it could have started a supervisor in at any
/// moment. Re-spawning on every failed dial is the failure the original comment named and is still
/// refused: it aims a fork bomb at the project whose supervisor is two syscalls from binding.
///
/// So a retry needs *evidence*, and it has two kinds. The chain's ready pipe has ended, so the
/// attempt is over, not slow. And [`crate::socket::nobody_is_serving`] — the same `flock` that
/// decides who serves — proves no supervisor holds the lock, which is precisely what a stillborn
/// stage 3 leaves behind. Three is where a bound stops being a retry and starts being a loop.
const MAX_START_ATTEMPTS: usize = 3;

/// The same three readings `socket::acquire` treats as "nobody answered", and for the same reason:
/// none of them is conclusive on its own, which is why the lock and not the dial decides.
fn nobody_answered(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ConnectionRefused | ErrorKind::NotFound | ErrorKind::WouldBlock
    )
}

/// Start stage 1 and **wait for it**, so nothing is left unreaped in the launcher, then wait on the
/// chain's ready pipe until `deadline`.
///
/// Waiting on stage 1 costs one process lifetime — stage 1 exits as soon as stage 2 does, and stage
/// 2 exits as soon as it has spawned stage 3 — and buys the property that `marion run` never
/// accumulates zombies. It does *not* wait for the supervisor, which outlives all of this by
/// construction: the ready pipe does. It is stage 1's stdout, handed down every stage under
/// [`READY_FLAG`], so it is readable once stage 3 writes its byte ([`tell_ready`]) or once every
/// stage holding it has exited without one. `poll(2)` blocks on it; nothing re-asks on a timer.
///
/// The pipe is `Stdio::piped()`, so it is made inside the spawn and no other thread's child can
/// inherit a copy that would hold its end open.
fn start_and_wait(launch: &Launch, deadline: Instant) -> Result<(), DetachError> {
    let mut command = Command::new(&launch.program);
    command
        .args(launch.argv())
        .arg(READY_FLAG)
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    // stderr is inherited on stages 1 and 2 **only**: a supervisor that could not start must
    // say so where the operator who asked for it is looking. Stage 3 redirects it to a file
    // before it becomes long-lived — see [`spawn_stage_three`].
    let mut child = crate::spawn_receive_gate::SPAWN_RECEIVE_GATE
        .spawn(&mut command)
        .map_err(|source| DetachError::Spawn {
            program: launch.program.clone(),
            source,
        })?;
    let ready = child.stdout.take().expect("stdout was piped");
    let status = child.wait().map_err(|source| DetachError::Spawn {
        program: launch.program.clone(),
        source,
    })?;
    // Whatever stage 1 said: a stage killed after spawning the next leaves a supervisor on its way.
    wait_readable_until(&ready, deadline);
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

/// Block until `pipe` is readable — a byte, or its end — or `deadline` passes. A `poll` cut short by
/// a signal is resumed, so only the pipe or the deadline ends the wait.
fn wait_readable_until(pipe: &impl std::os::fd::AsFd, deadline: Instant) {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero()
            || crate::wake::wait_readable(&[pipe.as_fd()], Some(left))
                .first()
                .copied()
                .unwrap_or(true)
        {
            return;
        }
    }
}

/// Where a stage's child's stdout goes: the ready pipe it was itself given under [`READY_FLAG`], or
/// `/dev/null`.
fn ready_stdout(ready: bool) -> Stdio {
    if ready {
        Stdio::inherit()
    } else {
        Stdio::null()
    }
}

/// The argv a stage hands its child: [`Launch::argv`], the child's own marker, and [`READY_FLAG`]
/// when this stage was given the pipe.
fn stage_argv(launch: &Launch, marker: &str, ready: bool) -> Vec<String> {
    let mut argv = launch.argv();
    argv.push(marker.to_string());
    if ready {
        argv.push(READY_FLAG.to_string());
    }
    argv
}

/// Stage 1: spawn stage 2 and wait. See the module doc for why this hop exists at all.
pub fn run_stage_one(launch: &Launch, ready: bool) -> Result<(), DetachError> {
    let mut command = Command::new(&launch.program);
    command
        .args(stage_argv(launch, SESSION_LEADER_FLAG, ready))
        .stdin(Stdio::null())
        .stdout(ready_stdout(ready));
    let mut child = crate::spawn_receive_gate::SPAWN_RECEIVE_GATE
        .spawn(&mut command)
        .map_err(|source| DetachError::Spawn {
            program: launch.program.clone(),
            source,
        })?;
    let status = child.wait().map_err(|source| DetachError::Spawn {
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
pub fn run_stage_two(launch: &Launch, ready: bool) -> Result<(), DetachError> {
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
    spawn_stage_three(launch, ready)
}

/// Stage 2's fork: the supervisor itself, detached and not waited for.
fn spawn_stage_three(launch: &Launch, ready: bool) -> Result<(), DetachError> {
    let argv = stage_argv(launch, DETACHED_FLAG, ready);
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
    let mut command = Command::new(&launch.program);
    command
        .args(argv)
        .stdin(Stdio::null())
        // The ready pipe, until stage 3 has said its word and let it go ([`tell_ready`]).
        .stdout(ready_stdout(ready))
        .stderr(stderr)
        // `/` and not the launcher's cwd: see the module doc. A detached process holding a cwd pins
        // a directory that may be deleted, and marion's own path derivation reads `cwd`.
        .current_dir(Path::new("/"));
    crate::spawn_receive_gate::SPAWN_RECEIVE_GATE
        .spawn(&mut command)
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
/// The endpoint stage 3 will serve against, or `None` for a refusal.
///
/// The pair is checked as a pair, in the direction each mode makes true: `Canned` means marion
/// names the endpoint, `Inherited` means marion names none and the harness resolves its own. A
/// `--base-url` under `Inherited` would be a value stage 3 must then ignore, which is a flag that
/// says one thing and does another.
fn paired_base_url(auth: marion_harness::Auth, base_url: Option<String>) -> Option<Option<String>> {
    match (auth, base_url) {
        (marion_harness::Auth::Canned, Some(u)) if !u.trim().is_empty() => Some(Some(u)),
        (marion_harness::Auth::Canned, _) => None,
        (marion_harness::Auth::Inherited, None) => Some(None),
        (marion_harness::Auth::Inherited, Some(_)) => None,
        // Endpoint is a per-node resolution, never a supervisor's mode.
        (marion_harness::Auth::Endpoint, _) => None,
    }
}

pub fn parse_serve(program: PathBuf, argv: &[String]) -> Option<(Launch, Stage, bool)> {
    let mut state_dir: Option<PathBuf> = None;
    let mut project_root: Option<PathBuf> = None;
    let mut idle_grace: Option<Duration> = None;
    let mut auth: Option<marion_harness::Auth> = None;
    let mut base_url: Option<String> = None;
    let mut stage = Stage::One;
    let mut ready = false;
    let mut rest = argv.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--state-dir" => state_dir = Some(PathBuf::from(rest.next()?)),
            "--project-root" => project_root = Some(PathBuf::from(rest.next()?)),
            "--idle-grace-ms" => {
                idle_grace = Some(Duration::from_millis(rest.next()?.parse().ok()?))
            }
            // **`from_wire` and not a local match**, so `"live"` or `"true"` is a refusal here
            // exactly as it is in a declaration's `env` block, and neither spelling can acquire a
            // second meaning on this surface.
            AUTH_FLAG => auth = Some(marion_harness::Auth::from_wire(rest.next()?)?),
            BASE_URL_FLAG => base_url = Some(rest.next()?.clone()),
            SESSION_LEADER_FLAG => stage = Stage::SessionLeader,
            DETACHED_FLAG => stage = Stage::Detached,
            READY_FLAG => ready = true,
            _ => return None,
        }
    }
    // **An absent `--auth` is a refusal, not `Canned`.** This is the whole reason the mode travels
    // on argv rather than being re-derived in stage 3; see [`Launch::auth`]. `idle_grace` above
    // *does* default, and the difference is which way absence is wrong: an unstated grace costs a
    // supervisor that lingers five minutes and says so in §5.7's own terms, while an unstated auth
    // mode costs an operator's live fleet pointed at an endpoint nobody is running.
    let auth = auth?;
    let base_url = paired_base_url(auth, base_url)?;
    Some((
        Launch {
            program,
            state_dir: state_dir?,
            project_root: project_root?,
            idle_grace: idle_grace.unwrap_or(crate::serve::DEFAULT_IDLE_GRACE),
            auth,
            base_url,
        },
        stage,
        ready,
    ))
}

/// `marion-supervisor serve …`, whichever stage it is.
pub fn run_serve(program: PathBuf, argv: &[String]) -> Result<(), DetachError> {
    let Some((launch, stage, ready)) = parse_serve(program.clone(), argv) else {
        return Err(DetachError::Spawn {
            program,
            source: std::io::Error::other(format!(
                "usage: marion-supervisor {SERVE} --state-dir <dir> --project-root <dir> \
                 --auth <canned|inherited> [--base-url <url>] [--idle-grace-ms <n>]. `--auth` is \
                 required and `--base-url` is required with `canned` and refused with \
                 `inherited`: a supervisor that guessed either would run a fleet against an \
                 endpoint nobody chose. A supervisor is started on demand by the first client that \
                 dials and finds nothing listening; it is not normally typed."
            )),
        });
    };
    match stage {
        Stage::One => run_stage_one(&launch, ready),
        Stage::SessionLeader => run_stage_two(&launch, ready),
        Stage::Detached => run_stage_three(&launch, ready),
    }
}

/// Stage 3 — the supervisor. Check the identity, take the socket or stand down, and serve.
///
/// **Standing down is silent and journals nothing.** A stage 3 that loses `socket::acquire`'s race
/// gets [`Acquired::Dialed`](crate::socket::Acquired::Dialed), which means another supervisor holds
/// the lock — so this process never was one, has no clients, no nodes and no exit to record, and
/// §5.7's *"exit MUST be journaled"* does not apply to it. Writing a `SupervisorExited` here would
/// put a second supervisor's departure in a journal whose supervisor is still running.
pub fn run_stage_three(launch: &Launch, ready: bool) -> Result<(), DetachError> {
    use crate::handler::RegistryHandle;
    use crate::registry::{LiveRegistry, Registry};
    use crate::serve::{NativeLaunchConfig, Server};
    use crate::socket::{Acquired, acquire_notifying, socket_paths};
    use marion_core::paths::ProjectDir;

    ensure_detached(SupervisorIdentity::own()).map_err(|source| DetachError::Spawn {
        program: launch.program.clone(),
        source: std::io::Error::other(source.to_string()),
    })?;
    let paths = socket_paths(
        &launch.state_dir,
        &launch.project_root,
        crate::socket::own_uid(),
    );
    // Bound, or dialed: either way a supervisor is serving, and the chain's client may dial. A
    // failure says so by the pipe's end instead.
    let say_ready = || {
        if ready {
            tell_ready();
        }
    };
    let Acquired::Serving(serving) = acquire_notifying(&paths, say_ready)? else {
        say_ready();
        return Ok(());
    };
    let project = ProjectDir::new(&launch.state_dir, &launch.project_root);
    let mut registry = Registry::boot(&project).map_err(|source| DetachError::Spawn {
        program: launch.program.clone(),
        source: std::io::Error::other(format!(
            "the supervisor could not open this project's journal: {source}"
        )),
    })?;
    // Right after the boot, while this supervisor still has no queue of its own: every message
    // still queued on the journal died with the supervisor that held it (`restart`'s module docs).
    // Found by the boot's own fold, so the journal is read and decoded once. An audit record, so a
    // failure to write one is reported and does not keep the supervisor down.
    if let Err(e) = crate::restart::drop_undelivered_at_boot(&mut registry) {
        eprintln!(
            "marion-supervisor: could not journal the messages the previous supervisor left \
             queued: {e}"
        );
    }
    let live = std::sync::Arc::new(LiveRegistry::follow(registry));
    // The bound listener is the authority on where clients dial, not the journal's directory: §2
    // may have moved the socket to its `/tmp` fallback. See `RegistryHandle::owning`.
    let socket_path = serving.path().to_path_buf();
    let sentry = serving.sentry();
    // **`owning`, so §2's `agent/spawn` is answered rather than refused** (§11 item 28 step 5).
    //
    // Every field is either derived from what this launch was already told or carried on its argv,
    // and there is deliberately no repository among them: §2 keys this supervisor on the git common
    // dir, so it serves a repository *and every linked worktree of it*, and a supervisor-wide
    // `repo` would branch a feature worktree's children off the main tree's HEAD. The tree is a
    // per-spawn input — `run::SpawnRequest::repo`, resolved from the caller's own node entry.
    let env = crate::run::Env {
        // On unless the operator started marion with `MARION_SANDBOX=off`.
        os_sandbox: marion_harness::os_sandbox::enabled_by_operator(),
        project_dir: project.clone(),
        state: launch.state_dir.clone(),
        // §2's key, so a `node/resume` can rebuild a lost root's launch in the repository this
        // supervisor already serves. Not a spawn input — see the field's doc.
        project_root: launch.project_root.clone(),
        // The binary this process is, not a name looked up on a `$PATH` a detached process does not
        // have. Same fallback `main::spawn_env` takes, and for the same reason.
        bridge: std::env::current_exe().unwrap_or_else(|_| launch.program.clone()),
        base_url: launch.base_url.clone(),
        auth: launch.auth,
    };
    // The enabled native bootstrap service, on the private sibling socket. Its descriptor slice is
    // the production registry and its adapter table is the harness registry's own
    // (`marion_harness::native_adapter`: one row-derived adapter per harness that states a
    // launch-time declaration channel, `None` for ACP), the same composition the integration bed
    // exercises with a fixture table.
    let server = Server::start_with_native_launch(
        serving,
        RegistryHandle::owning(live, env.clone(), socket_path),
        NativeLaunchConfig {
            descriptors: marion_core::PRODUCTION_NATIVE_FACADES,
            adapter_for: marion_harness::native_adapter,
            env,
        },
        launch.idle_grace,
    )
    .map_err(|source| DetachError::Spawn {
        program: launch.program.clone(),
        source: std::io::Error::other(format!(
            "the supervisor could not install its native bootstrap service: {source}"
        )),
    })?;
    watch_entitlement(sentry);
    // §5.7: the supervisor's lifetime is not its client's. The only way out of this call is the
    // accept loop's own idle exit, which has already journaled the record by the time it returns.
    server.wait();
    Ok(())
}

/// Tell the client that started this chain that a supervisor is serving: one byte on the ready pipe
/// ([`READY_FLAG`]), then the pipe let go — `/dev/null` in its place — so the client's wait is not
/// held open for this process's life and nothing later writes into a pipe nobody reads.
fn tell_ready() {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let mut out = std::io::stdout();
    let _ = out.write_all(b"R").and_then(|()| out.flush());
    if let Ok(null) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
        // SAFETY: both descriptors are valid; `dup2` replaces fd 1 atomically.
        unsafe { dup2(null.as_raw_fd(), 1) };
    }
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
        // The lock and its directory, watched: removing `<state>/<hash>` unlinks the lock, and
        // removing or replacing a socket changes the directory's entries — vnode/inotify events
        // the kernel reports at once. The wait has no timeout; a watch that could not be armed
        // makes it re-check at the degraded bound instead.
        let mut watches: Vec<crate::wake::Watch> = sentry
            .watched_paths()
            .iter()
            .map(|p| crate::wake::Watch::new(p))
            .collect();
        loop {
            let fds: Vec<_> = watches.iter().map(|w| w.fd()).collect();
            crate::wake::wait_until(&fds, None);
            drop(fds);
            for watch in &mut watches {
                watch.rearm();
            }
            if sentry.still_entitled() {
                continue;
            }
            eprintln!("marion-supervisor: standing down -- {}", sentry.why());
            std::process::exit(70);
        }
    });
}

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

    /// A client that cannot reach the supervisor it started says so without citing a design
    /// section to a person who has never read it.
    #[test]
    fn an_unreachable_supervisor_names_no_spec_section() {
        let said = DetachError::NotReachable {
            path: PathBuf::from("/s/p/supervisor.sock"),
            waited_ms: 5000,
        }
        .to_string();
        assert!(said.contains("/s/p/supervisor.sock"), "{said}");
        assert!(!said.contains('§'), "{said}");
    }

    /// **A `/tmp` fallback directory somebody else controls is refused before anything starts**,
    /// with the way out: a supervisor started into it could not bind or even open its log, and
    /// the client would wait out its whole bound to report something unrelated.
    #[test]
    fn a_fallback_directory_somebody_else_controls_is_refused_before_anything_starts() {
        use std::os::unix::fs::PermissionsExt;
        // A uid nobody has, so the fallback directory is this test's own.
        let uid = 4_000_000_000 + std::process::id();
        let squatted = PathBuf::from(format!("/tmp/marion-{uid}"));
        let _ = std::fs::remove_dir_all(&squatted);
        std::fs::create_dir(&squatted).unwrap();
        std::fs::set_permissions(&squatted, std::fs::Permissions::from_mode(0o777)).unwrap();
        let dir = marion_testsupport::scratch("detach-squatted");
        let state = dir.join("s".repeat(120));
        let paths = crate::socket::socket_paths(&state, Path::new("/p/.git"), uid);
        assert!(
            paths.overflow().is_some(),
            "the long state path takes the fallback"
        );
        let launch = Launch {
            program: dir.join("no-such-supervisor"),
            state_dir: state,
            project_root: PathBuf::from("/p/.git"),
            idle_grace: Duration::from_millis(250),
            auth: marion_harness::Auth::Canned,
            base_url: None,
        };
        let e =
            ensure_supervisor(&paths, &launch).expect_err("nothing may start into that directory");
        let _ = std::fs::remove_dir_all(&squatted);
        assert!(
            matches!(e, DetachError::Socket(SocketError::UnsafeDir { .. })),
            "the directory is the diagnosis, not a failed start: {e}"
        );
        assert!(
            e.to_string().contains("MARION_STATE_DIR"),
            "and the way out: {e}"
        );
    }

    /// The three stages carry **one** description of the project between them, so stage 3 cannot
    /// serve a different socket than the client that asked for it dialed.
    #[test]
    fn every_stage_is_told_the_same_project_and_only_the_marker_differs() {
        let launch = Launch {
            program: PathBuf::from("/bin/marion-supervisor"),
            state_dir: PathBuf::from("/s"),
            project_root: PathBuf::from("/p/.git"),
            idle_grace: Duration::from_millis(250),
            auth: marion_harness::Auth::Canned,
            base_url: Some("http://127.0.0.1:8099/v1".into()),
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
