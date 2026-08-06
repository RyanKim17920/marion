//! The unix socket: where it lives, and who gets to serve it (design §2, §4.3, §5.7, §10).
//!
//! This module answers two questions and nothing else. *Which path?* — §2's keying rule, with the
//! `sun_path` length check and the `/tmp` fallback that `marion_core::paths` deliberately left
//! here. *Who binds it?* — §5.7's start race, made idempotent under contention.
//!
//! # The path moves in exactly one respect: it does not
//!
//! §10's table moves socket **ownership** once — the `marion` process in M1, a detached
//! `marion-supervisor` in M2+ — and says the path is *"identical in both … which is what makes M2's
//! split invisible to children."* So the derivation here is written as one pure function of
//! `(state, project root, uid)` with an environment-reading wrapper on top, and the per-child MCP
//! bridge is expected to call the same function rather than to re-derive the rule. A second
//! implementation of a rule two *processes* must agree on is the failure mode this shape exists to
//! prevent.
//!
//! # Why the length limit is a constant and not the platform's
//!
//! `sockaddr_un.sun_path` is 104 bytes on macOS and 108 on Linux. [`SUN_PATH_MAX`] is 104
//! everywhere, which is what §2 names. The reason to pin it rather than read it per platform is the
//! sentence above: the supervisor and the bridge are separate processes deriving one path, and a
//! rule whose output depends on a platform constant is a rule they could compile differently. 104
//! is also the conservative bound, so a path that fits it fits everywhere marion runs.
//!
//! # Why the start race is a lockfile and not bind-then-rename
//!
//! §5.7 names both and settles neither — *"which one marion uses is not settled here and has not
//! been measured"* — while making the **outcome** normative: one supervisor, and the loser dials
//! the winner rather than erroring. marion uses an `flock`'d lockfile beside the socket. Four
//! reasons, in the order they decided it:
//!
//! 1. **`rename(2)` overwrites.** It is atomic, but atomicity is not exclusion: the loser's rename
//!    silently replaces the winner's socket file, leaving the winner listening on an unlinked inode
//!    while every subsequent client dials the loser's. Making bind-then-publish a *race* at all
//!    needs `link(2)` and its `EEXIST`, not `rename`.
//! 2. **Neither rename nor link answers the stale socket.** A supervisor that died leaves its file
//!    behind, and taking it over means unlinking a path that — from the unlinker's side — might
//!    belong to a server that is merely slow. The lock answers it structurally: **a server holds
//!    the lock for its entire serving life**, so acquiring the lock *is* the proof that no server
//!    exists, and therefore that any file at the path is stale. Nothing here ever unlinks a socket
//!    without that proof.
//! 3. **A crash cleans up after itself.** The kernel drops an `flock` when the holding process
//!    dies, however it dies. An `O_EXCL` sentinel would instead need pid-liveness heuristics to
//!    tell a crashed supervisor's leftovers from a live one's.
//! 4. **The length budget is checked against the name that is actually bound.** Bind-then-rename
//!    would have to fit a *temporary* name inside 103 bytes as well, so the fallback would trigger
//!    at a different length than the rule §2 states.
//!
//! There is a fifth benefit that only shows up under load. A listener whose backlog is full answers
//! `connect` with `ECONNREFUSED` on Linux and `EAGAIN` on macOS — which is to say, a **busy** server
//! is indistinguishable from **no** server by dialing alone. A caller that took `ECONNREFUSED` as
//! permission to rebind would steal the socket out from under a working supervisor at exactly its
//! busiest moment. Here `ECONNREFUSED` only sends the caller to the lock, which is held, so it
//! retries the dial. The distinction §5.7 needs — *nobody is listening* versus *someone is listening
//! and wedged* — is the lock, never the dial.
//!
//! # Why [`acquire`] loops instead of asking once
//!
//! **Measured on darwin 25.5.0 while writing this module's tests, and it is the reason the loop
//! exists.** Closing a descriptor is not synchronous with respect to another descriptor's view of
//! what it held. For a sub-millisecond window after a listener is closed, `connect` to its path
//! still *succeeds*; for the same kind of window after an `flock` holder closes, `flock(LOCK_NB)`
//! from a fresh descriptor still answers `EWOULDBLOCK` — the retry one millisecond later succeeded
//! on the first attempt, every time.
//!
//! A caller that asked once and believed the answer would therefore conclude that a supervisor
//! which has just exited is alive, or that a lock nobody holds is held — and §5.7's rule is that the
//! loser dials rather than *"erroring or starting a second"*. Neither reading is stable, so
//! [`acquire`] treats a single negative as provisional and re-asks. This is also why nothing here
//! and nothing in the tests asserts on elapsed time: the loop is what makes the window invisible,
//! and the properties asserted are counts and outcomes.
//!
//! # What this module does not do
//!
//! It does not detach. S15 chose double-`fork` + `setsid` and measured what one tree-wide signal
//! reaches; wiring that is a separate change. Nothing here forecloses it: [`acquire`] is the same
//! call for an in-process M1 supervisor and a detached M2 one, and neither the path nor the lock
//! depends on process group or session membership.

use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marion_core::paths::{ProjectDir, project_hash, state_dir};

unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
    fn getuid() -> u32;
}

const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;

/// `sizeof(sockaddr_un.sun_path)` as §2 states it. See the module doc for why it is not read from
/// the platform.
pub const SUN_PATH_MAX: usize = 104;

/// The longest socket path that fits, in bytes. One less than [`SUN_PATH_MAX`], because the kernel
/// wants the path NUL-terminated *inside* the field — an off-by-one here does not fail at bind
/// time on every platform, it truncates, and a truncated path is a second socket nobody dials.
pub const MAX_SOCKET_PATH_BYTES: usize = SUN_PATH_MAX - 1;

/// How long [`acquire`] will keep trying before it reports that it could neither serve nor dial.
///
/// **A bound, not a measurement.** The window it covers is the handful of syscalls between one
/// process taking the lock and that process finishing its `bind` — microseconds — and this is
/// generous enough that a loaded machine never decides the answer. Nothing asserts on how long
/// `acquire` takes; the tests assert on *how many* callers ended up serving.
const START_DEADLINE: Duration = Duration::from_secs(5);

/// Where the socket, its lock and their directory are, and whether §2's fallback was taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketPaths {
    dir: PathBuf,
    socket: PathBuf,
    lock: PathBuf,
    /// `Some(reason)` iff the `<state>` path did not fit [`MAX_SOCKET_PATH_BYTES`]. A sentence and
    /// not a `bool`: §2 calls a long `$HOME` under `$XDG_STATE_HOME` *"a normal case, not an exotic
    /// one"*, so an operator who finds their socket somewhere unexpected deserves to be told which
    /// path overflowed and by how much.
    overflow: Option<String>,
}

impl SocketPaths {
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The `flock` file. Beside the socket, never inside a different directory: the two must share
    /// a filesystem lifetime, or a lock could survive a state directory that was removed.
    pub fn lock(&self) -> &Path {
        &self.lock
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether §2's `/tmp` fallback was taken, and why.
    pub fn overflow(&self) -> Option<&str> {
        self.overflow.as_deref()
    }
}

/// §2's socket path, as pure arithmetic over `(state, canonical project root, uid)`.
///
/// No environment, no filesystem, no `git`: everything that has to be *looked up* is looked up by
/// [`resolve`] and handed in, which is `marion_core::paths`' rule applied one layer out and what
/// makes the overflow branch testable without a 100-byte `$HOME`.
///
/// `<project-hash>` is the same 12 hex characters in both branches — deliberately, so the fallback
/// keys on the same project the primary path does and two projects cannot collide in `/tmp` any
/// more easily than they can under `<state>`.
pub fn socket_paths(state: &Path, canonical_root: &Path, uid: u32) -> SocketPaths {
    let project = ProjectDir::new(state, canonical_root);
    let primary = project.supervisor_sock();
    let len = primary.as_os_str().as_encoded_bytes().len();
    if len <= MAX_SOCKET_PATH_BYTES {
        return SocketPaths {
            dir: project.path().to_path_buf(),
            lock: project.path().join("supervisor.lock"),
            socket: primary,
            overflow: None,
        };
    }
    let hash = project_hash(canonical_root);
    let dir = PathBuf::from(format!("/tmp/marion-{uid}"));
    SocketPaths {
        socket: dir.join(format!("{hash}.sock")),
        lock: dir.join(format!("{hash}.lock")),
        dir,
        overflow: Some(format!(
            "{} is {len} bytes and a unix socket path may be at most {MAX_SOCKET_PATH_BYTES} \
             (sun_path is {SUN_PATH_MAX} including its NUL), so marion put this project's socket \
             under /tmp instead (§2). The per-child MCP bridge derives the same path by the same \
             rule, so nothing has to be told.",
            primary.display()
        )),
    }
}

/// The **project root** §2 keys on: git's common directory, canonicalized, falling back to `cwd`.
///
/// The common dir and not the worktree: §2 says so, and gives the reason — *"worktree children
/// (§6.6) have different cwds and would otherwise hash to different supervisors"*. `--git-common-dir`
/// is the one spelling that answers the *main* repository's `.git` from inside a linked worktree,
/// which is exactly the case that motivated the rule.
///
/// A directory that is not in a repository at all falls back to `cwd`, per §2. That is a weaker key
/// — two shells in two subdirectories of one non-repo project get two supervisors — and it is the
/// spec's choice, because the alternative is refusing to run outside git.
pub fn project_root(cwd: &Path) -> PathBuf {
    let common = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| {
            let p = Path::new(&s).to_path_buf();
            if p.is_absolute() { p } else { cwd.join(p) }
        });
    let chosen = common.unwrap_or_else(|| cwd.to_path_buf());
    chosen.canonicalize().unwrap_or(chosen)
}

/// [`socket_paths`], with the environment read for it.
///
/// The three `<state>` variables are read here rather than inside `marion-core` for the reason
/// `paths.rs` gives at length: keeping the arithmetic pure is what makes the precedence rule
/// testable without mutating process-global environment state, which is a data race in a threaded
/// test binary.
pub fn resolve(cwd: &Path) -> Result<SocketPaths, SocketError> {
    let state = state_dir(
        std::env::var("MARION_STATE_DIR").ok().as_deref(),
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
    .ok_or(SocketError::NoStateDir)?;
    Ok(socket_paths(
        &state,
        &project_root(cwd),
        // SAFETY: `getuid` reads the calling process's real uid and cannot fail.
        unsafe { getuid() },
    ))
}

/// What [`acquire`] settled: this caller serves, or someone else already does.
///
/// One enum and one call, because §5.7 requires that in a race *"one supervisor wins, the loser
/// dials the winner rather than erroring or starting a second"* — a caller that had to ask "am I
/// first?" and then act on the answer would have a window between the two.
#[derive(Debug)]
pub enum Acquired {
    /// This caller holds the lock and the listener. Nobody else can reach this arm until it drops.
    Serving(Serving),
    /// Somebody else is serving, and here is the connection to them.
    Dialed(UnixStream),
}

/// A bound listener and the lock that entitles it, which is why they are one value.
///
/// Splitting them would let a caller drop the lock and keep serving, and the moment that is
/// possible a second supervisor can bind the same project's journal — the outcome §5.7 says the
/// split was for.
#[derive(Debug)]
pub struct Serving {
    listener: UnixListener,
    path: PathBuf,
    /// Held for the whole serving life. Never read; the file descriptor's existence *is* the lock.
    _lock: std::fs::File,
}

impl Serving {
    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Serving {
    /// Unlink on the way out, so a supervisor that ends cleanly does not leave a path that dials to
    /// `ECONNREFUSED`. Safe without further checks for the reason the module doc gives: this value
    /// exists only while the lock is held, and the lock is what proves no other server is there.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SocketError {
    #[error(
        "marion could not resolve a state directory: none of $MARION_STATE_DIR, $XDG_STATE_HOME \
         or $HOME is set (§4.3). It has nowhere to put this project's socket, and guessing one \
         would put a supervisor somewhere a second invocation would not look."
    )]
    NoStateDir,
    #[error("marion could not create {path}: {source}")]
    Dir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "{path} already exists and is not a private directory owned by this user ({detail}). It \
         holds a socket that grants full control of this project's fleet, and /tmp is writable by \
         everyone, so marion refuses to bind inside it rather than serve through a directory \
         somebody else can replace."
    )]
    UnsafeDir { path: PathBuf, detail: String },
    #[error("marion could not take {path}: {source}")]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("marion could not bind {path}: {source}")]
    Bind {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "{path} could not be dialed and could not be taken over within {waited_ms} ms: something \
         holds the supervisor lock for this project without serving the socket. marion did not \
         unlink the socket, because the lock says a supervisor may still be there."
    )]
    Wedged { path: PathBuf, waited_ms: u128 },
    #[error("marion could not dial {path}: {source}")]
    Dial {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// §5.7's start, made idempotent under contention: **exactly one caller serves and every other one
/// is connected to it.**
///
/// The loop is three steps and each failure is a different next move:
///
/// 1. **Dial.** A connection means a supervisor is serving; done, and nothing was touched.
/// 2. **`ECONNREFUSED` or no file** — nobody answered, which is *not yet* evidence that nobody is
///    there (see the module doc on full backlogs). Try the lock.
/// 3. **The lock is held** — a supervisor is serving or is a few syscalls from serving. Go back to
///    step 1. **The lock is taken** — no supervisor exists, so the socket file, if any, is stale;
///    unlink it and bind.
///
/// The only outcome this can fail with, short of a real filesystem error, is [`SocketError::Wedged`]
/// — the lock held by something that never binds. marion reports that rather than unlinking, which
/// is the one move that could take a socket away from a supervisor that is merely slow.
pub fn acquire(paths: &SocketPaths) -> Result<Acquired, SocketError> {
    acquire_within(paths, START_DEADLINE)
}

/// [`acquire`] with the bound named, so a test that wants to observe the *refusal* need not wait
/// out a bound written for a machine under load. The bound never decides a verdict — a lock held
/// for the whole call answers [`SocketError::Wedged`] at any bound, and a lock that is free is
/// taken on the first pass — which is why exposing it cannot make a test measure the machine.
pub fn acquire_within(paths: &SocketPaths, within: Duration) -> Result<Acquired, SocketError> {
    let uid = unsafe { getuid() };
    ensure_dir(paths, uid)?;
    let deadline = Instant::now() + within;
    loop {
        match UnixStream::connect(paths.socket()) {
            Ok(s) => return Ok(Acquired::Dialed(s)),
            Err(e) if nobody_answered(&e) => {}
            Err(e) => {
                return Err(SocketError::Dial {
                    path: paths.socket().to_path_buf(),
                    source: e,
                });
            }
        }
        match take_lock(paths.lock())? {
            Some(lock) => {
                // The lock is the proof. See the module doc: a server holds it for its whole
                // serving life, so nothing is listening on this path and the file is a corpse.
                match std::fs::remove_file(paths.socket()) {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(SocketError::Bind {
                            path: paths.socket().to_path_buf(),
                            source: e,
                        });
                    }
                }
                let listener =
                    UnixListener::bind(paths.socket()).map_err(|e| SocketError::Bind {
                        path: paths.socket().to_path_buf(),
                        source: e,
                    })?;
                // 0600: the socket is the fleet's control plane and §5.4's authorization model is
                // "the client is the operator". File mode is the only thing that says which
                // operator.
                let _ = std::fs::set_permissions(
                    paths.socket(),
                    std::fs::Permissions::from_mode(0o600),
                );
                return Ok(Acquired::Serving(Serving {
                    listener,
                    path: paths.socket().to_path_buf(),
                    _lock: lock,
                }));
            }
            None => {
                if Instant::now() >= deadline {
                    return Err(SocketError::Wedged {
                        path: paths.socket().to_path_buf(),
                        waited_ms: within.as_millis(),
                    });
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

/// A dial that reached nobody. Both readings arrive here and neither is conclusive on its own —
/// which is the whole reason the lock exists rather than this predicate deciding anything.
fn nobody_answered(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ConnectionRefused | ErrorKind::NotFound | ErrorKind::WouldBlock
    )
}

fn take_lock(path: &Path) -> Result<Option<std::fs::File>, SocketError> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|e| SocketError::Lock {
            path: path.to_path_buf(),
            source: e,
        })?;
    // SAFETY: `f` owns a valid descriptor for the duration of the call.
    if unsafe { flock(f.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
        return Ok(Some(f));
    }
    let e = std::io::Error::last_os_error();
    if e.kind() == ErrorKind::WouldBlock {
        // Somebody else holds it. Not an error — it is the answer.
        return Ok(None);
    }
    Err(SocketError::Lock {
        path: path.to_path_buf(),
        source: e,
    })
}

/// The lock file is **never unlinked**, including by [`Serving::drop`].
///
/// Removing it would be the classic mistake: two processes that `open` the same path either side of
/// an unlink hold descriptors to two different inodes, `flock` them independently, and both
/// conclude they are alone. An empty 0600 file left behind costs nothing and is what keeps the
/// exclusion honest across a restart.
fn ensure_dir(paths: &SocketPaths, uid: u32) -> Result<(), SocketError> {
    let dir = paths.dir();
    match std::fs::symlink_metadata(dir) {
        Ok(md) => {
            // Only the `/tmp` branch is *audited*. Under `<state>` the parent chain is the user's
            // own `$HOME`/`$XDG_STATE_HOME` and the directory may predate this code with whatever
            // mode `marion run` gave it; under `/tmp` the parent is world-writable and the check is
            // the only thing between the fleet's control plane and anyone with a shell.
            if paths.overflow().is_some() {
                let mut faults = Vec::new();
                if md.is_symlink() {
                    faults.push("it is a symlink".to_string());
                } else if !md.is_dir() {
                    faults.push("it is not a directory".to_string());
                }
                if md.uid() != uid {
                    faults.push(format!("it is owned by uid {} and not {uid}", md.uid()));
                }
                if md.permissions().mode() & 0o077 != 0 {
                    faults.push(format!(
                        "its mode is {:o} and group or other can write into it",
                        md.permissions().mode() & 0o7777
                    ));
                }
                if !faults.is_empty() {
                    return Err(SocketError::UnsafeDir {
                        path: dir.to_path_buf(),
                        detail: faults.join("; "),
                    });
                }
            }
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::NotFound => std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| SocketError::Dir {
                path: dir.to_path_buf(),
                source: e,
            }),
        Err(e) => Err(SocketError::Dir {
            path: dir.to_path_buf(),
            source: e,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_testsupport::scratch;
    use std::sync::{Arc, Barrier};

    /// §2/§4.3's path, and the `/tmp` fallback, from one rule with no environment involved.
    #[test]
    fn the_socket_path_is_the_project_dirs_until_it_does_not_fit() {
        let root = Path::new("/Users/u/code/marion");
        let hash = project_hash(root);

        let short = socket_paths(Path::new("/s"), root, 501);
        assert_eq!(
            short.socket(),
            Path::new("/s").join(&hash).join("supervisor.sock")
        );
        assert_eq!(
            short.lock(),
            Path::new("/s").join(&hash).join("supervisor.lock")
        );
        assert_eq!(short.dir(), Path::new("/s").join(&hash));
        assert_eq!(short.overflow(), None);
        // The same path `marion_core` computes: one derivation, not two.
        assert_eq!(
            short.socket(),
            ProjectDir::new(Path::new("/s"), root).supervisor_sock()
        );

        // §2 calls a long `$HOME` under `$XDG_STATE_HOME` a normal case. This is one.
        let long = Path::new(
            "/Users/a-rather-long-account-name/Library/Application Support/state/marion-supervisor",
        );
        let over = socket_paths(long, root, 501);
        assert_eq!(
            over.socket(),
            PathBuf::from(format!("/tmp/marion-501/{hash}.sock"))
        );
        assert_eq!(
            over.lock(),
            PathBuf::from(format!("/tmp/marion-501/{hash}.lock"))
        );
        assert_eq!(over.dir(), Path::new("/tmp/marion-501"));
        let why = over.overflow().expect("the fallback says why it was taken");
        assert!(why.contains("103"), "{why}");
        assert!(why.contains(&long.display().to_string()), "{why}");
        // The fallback keys on the *same* project, so two projects collide in /tmp no more easily
        // than they do under `<state>`.
        assert!(over.socket().to_string_lossy().contains(&hash));
    }

    /// **NC — the length check is exact, and it is checked on the byte that matters.**
    ///
    /// 103 bytes fits and 104 does not. An off-by-one in the permissive direction does not fail at
    /// bind time on every platform — it silently *truncates* the path, which produces a second
    /// socket at a name nobody dials, and a client that then finds nothing listening starts a
    /// second supervisor. That is why this asserts on the boundary rather than on "a long path
    /// falls back".
    #[test]
    fn a_path_of_exactly_the_limit_fits_and_one_byte_more_does_not() {
        // Pinned to literals, not derived from the constants — a test that computed its boundary
        // from `MAX_SOCKET_PATH_BYTES` would move with an off-by-one instead of catching it, which
        // is exactly what a mutation check found.
        assert_eq!(SUN_PATH_MAX, 104, "§2 names 104");
        assert_eq!(
            MAX_SOCKET_PATH_BYTES, 103,
            "one less than sun_path, because the kernel wants the NUL inside the field"
        );
        let root = Path::new("/r");
        let hash = project_hash(root);
        // "/supervisor.sock" is 16 bytes, the hash is 12, and the separator before it is 1.
        let tail = 1 + hash.len() + "/supervisor.sock".len();
        for (state_len, want_fallback) in [
            (MAX_SOCKET_PATH_BYTES - tail, false),
            (MAX_SOCKET_PATH_BYTES - tail + 1, true),
        ] {
            let state = PathBuf::from(format!("/{}", "x".repeat(state_len - 1)));
            let p = socket_paths(&state, root, 7);
            let len = p.socket().as_os_str().as_encoded_bytes().len();
            assert_eq!(
                p.overflow().is_some(),
                want_fallback,
                "a {} byte state dir gives a {len} byte socket path",
                state.as_os_str().as_encoded_bytes().len()
            );
            if !want_fallback {
                assert_eq!(
                    len, MAX_SOCKET_PATH_BYTES,
                    "the boundary case is at the limit"
                );
            }
        }
    }

    /// The path is a function of the project root and nothing else about the caller — which is what
    /// lets the per-child MCP bridge, a different process with a different cwd, find the socket its
    /// parent bound.
    #[test]
    fn two_callers_in_one_project_derive_one_socket_and_two_projects_do_not() {
        let a = socket_paths(Path::new("/s"), Path::new("/p/.git"), 1);
        let b = socket_paths(Path::new("/s"), Path::new("/p/.git"), 999);
        assert_eq!(a.socket(), b.socket(), "uid does not key the primary path");
        let other = socket_paths(Path::new("/s"), Path::new("/q/.git"), 1);
        assert_ne!(a.socket(), other.socket());
    }

    /// §2 keys on the **git common dir**, so a linked worktree resolves to its main repository's
    /// socket rather than to one of its own. §6.6's worktree children are exactly the case.
    #[test]
    fn a_linked_worktree_resolves_to_the_same_project_root_as_its_main_repo() {
        let dir = scratch("socket-worktree");
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("f"), b"x").unwrap();
        git(&repo, &["add", "f"]);
        git(&repo, &["commit", "-qm", "one"]);
        let wt = dir.join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "side"],
        );

        let from_repo = project_root(&repo);
        let from_worktree = project_root(&wt);
        assert_eq!(
            from_repo, from_worktree,
            "a worktree child would otherwise hash to a different supervisor (§2)"
        );
        assert_eq!(
            socket_paths(Path::new("/s"), &from_repo, 1).socket(),
            socket_paths(Path::new("/s"), &from_worktree, 1).socket()
        );
        // And the key really is the common dir, not the worktree's own cwd.
        assert_ne!(from_repo, repo.canonicalize().unwrap());

        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "remove", "--force", wt.to_str().unwrap()])
            .output();
    }

    /// Outside a repository §2 falls back to `cwd` rather than refusing to run.
    #[test]
    fn a_directory_outside_a_repository_keys_on_its_own_path() {
        let dir = scratch("socket-norepo");
        let sub = dir.join("plain");
        std::fs::create_dir_all(&sub).unwrap();
        // Only meaningful if the scratch dir is genuinely not inside a repo; `temp_dir()` is not.
        if std::process::Command::new("git")
            .arg("-C")
            .arg(&sub)
            .args(["rev-parse", "--git-common-dir"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return;
        }
        assert_eq!(project_root(&sub), sub.canonicalize().unwrap());
    }

    /// A scratch directory with a **short** path.
    ///
    /// `marion_testsupport::scratch` is rooted at `std::env::temp_dir()`, which on macOS is a
    /// ~50-byte `/private/var/folders/…` path — and a socket under it plus a tag plus a pid
    /// overruns the 103 bytes this module is about. That is the module's subject matter arriving
    /// in its own tests, and it is not a reason to relax the check: a test that binds a socket has
    /// to live somewhere a socket fits.
    struct ShortDir(PathBuf);

    impl ShortDir {
        fn new(tag: &str) -> Self {
            let p = PathBuf::from(format!("/tmp/mr-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("scratch dir");
            assert!(
                p.join("supervisor.sock").as_os_str().len() <= MAX_SOCKET_PATH_BYTES,
                "the test's own scratch path must fit the limit it is testing"
            );
            Self(p)
        }

        fn join(&self, leaf: &str) -> PathBuf {
            self.0.join(leaf)
        }
    }

    impl std::ops::Deref for ShortDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ShortDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn paths_in(dir: &Path) -> SocketPaths {
        SocketPaths {
            dir: dir.to_path_buf(),
            socket: dir.join("supervisor.sock"),
            lock: dir.join("supervisor.lock"),
            overflow: None,
        }
    }

    /// Wait for a condition, checking often, up to a bound generous enough that a loaded machine
    /// does not decide the answer — `registry.rs`'s helper, and used here for the same reason it is
    /// used there: to assert that a transition **happens**, never how long it takes.
    fn until(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        cond()
    }

    /// What a SIGKILLed supervisor leaves: a socket file with nobody behind it.
    ///
    /// The `until` is not decoration and it is the reason [`acquire`] retries at all. **Measured
    /// while writing these tests, on darwin 25.5.0**: closing a descriptor is not synchronous with
    /// respect to another descriptor's view of it. For a window of under a millisecond after the
    /// listener is closed, `connect` still succeeds — and, on the lock file, `flock(LOCK_NB)` from a
    /// fresh descriptor still answers `EWOULDBLOCK` after the holder closed (the *very next*
    /// attempt, one millisecond later, succeeded). A caller that asked once and believed the answer
    /// would conclude a dead supervisor is alive, or a free lock is held. `acquire`'s loop is what
    /// makes that window invisible in production; here the test has to wait it out explicitly
    /// before it can *pose* the question.
    fn leave_a_corpse(p: &SocketPaths) {
        {
            let _corpse = UnixListener::bind(p.socket()).expect("bind the corpse");
        }
        assert!(
            until(|| UnixStream::connect(p.socket()).is_err()),
            "a socket whose listener is closed must end up refusing rather than accepting"
        );
        assert!(p.socket().exists(), "the stale file is still there");
    }

    /// The ordinary case: nobody is there, so this caller serves; a second caller dials it.
    #[test]
    fn the_first_caller_serves_and_the_second_dials_rather_than_starting_a_second() {
        let dir = ShortDir::new("first");
        let p = paths_in(&dir);
        let Acquired::Serving(server) = acquire(&p).unwrap() else {
            panic!("nothing was listening, so this caller had to serve")
        };
        assert_eq!(server.path(), p.socket());
        assert!(p.socket().exists());

        let Acquired::Dialed(_client) = acquire(&p).unwrap() else {
            panic!("a supervisor is serving; a second one must not be started")
        };
        // And the server really is accepting, not merely bound.
        assert!(server.listener().accept().is_ok());
    }

    /// **NC — two racing starts are provably one supervisor.**
    ///
    /// Sixteen threads released together by a `Barrier`. The assertion is a *count*, not a timing:
    /// exactly one `Serving` and fifteen `Dialed`, and no errors at all — §5.7 requires that the
    /// loser dial rather than see a spurious failure. A single-threaded test cannot measure this,
    /// and neither can a test that starts the threads in sequence.
    #[test]
    fn sixteen_simultaneous_starts_produce_one_supervisor_and_no_spurious_failures() {
        const N: usize = 16;
        let dir = ShortDir::new("race");
        let p = paths_in(&dir);
        let gate = Arc::new(Barrier::new(N));
        let outcomes: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..N)
                .map(|_| {
                    let gate = Arc::clone(&gate);
                    let p = p.clone();
                    s.spawn(move || {
                        gate.wait();
                        acquire(&p)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let mut serving = 0usize;
        let mut dialed = 0usize;
        for o in &outcomes {
            match o {
                Ok(Acquired::Serving(_)) => serving += 1,
                Ok(Acquired::Dialed(_)) => dialed += 1,
                Err(e) => panic!("a loser must dial the winner, not fail: {e}"),
            }
        }
        assert_eq!(serving, 1, "§5.7: one supervisor wins, and only one");
        assert_eq!(dialed, N - 1, "every loser reached the winner");
    }

    /// **NC — a stale socket dials to a refusal and is taken over, once.**
    ///
    /// A supervisor that died leaves the path behind. Dialing it must not hang and must not be
    /// mistaken for a live server; taking it over must not happen twice. The second `acquire` here
    /// is the half that matters: it dials the new server rather than unlinking it in turn.
    #[test]
    fn a_socket_left_behind_by_a_dead_supervisor_is_refused_then_taken_over_once() {
        let dir = ShortDir::new("stale");
        let p = paths_in(&dir);
        {
            let Acquired::Serving(_dead) = acquire(&p).unwrap() else {
                panic!("first")
            };
        }
        // A clean drop unlinks. Recreate the corpse by hand, which is what a SIGKILLed supervisor
        // leaves: a socket inode with nobody behind it.
        leave_a_corpse(&p);

        let Acquired::Serving(_live) = acquire(&p).unwrap() else {
            panic!("nobody was listening, so this caller takes the stale path over")
        };
        let Acquired::Dialed(_) = acquire(&p).unwrap() else {
            panic!("the second caller must dial the new server, not unlink it as stale too")
        };
    }

    /// **NC — a socket somebody is serving is never unlinked, even when the dial fails.**
    ///
    /// The failure this guards is the one a `bool` predicate on `ECONNREFUSED` would cause: a
    /// listener whose backlog is full refuses connections, and a caller that read that as "nobody
    /// is there" would rebind over a working supervisor at its busiest moment. Here the lock is
    /// held, so the caller never reaches the unlink at all and reports [`SocketError::Wedged`]
    /// instead.
    #[test]
    fn a_locked_path_is_reported_rather_than_stolen() {
        let dir = ShortDir::new("wedged");
        let p = paths_in(&dir);
        // A supervisor that took the lock and has not bound yet — the window `acquire` polls
        // across, held open for longer than it will wait.
        let held = take_lock(p.lock()).unwrap().expect("the lock is free");
        let inode_before = std::fs::symlink_metadata(p.lock()).unwrap().ino();

        leave_a_corpse(&p);
        // The bound is named only so the *refusal* need not be waited out. It cannot decide the
        // verdict: the lock is held for the whole call, so `Wedged` is the only reachable answer at
        // any bound whatsoever.
        let e = acquire_within(&p, Duration::from_millis(20)).unwrap_err();
        assert!(
            matches!(e, SocketError::Wedged { .. }),
            "a held lock must be reported, never overridden: {e}"
        );
        assert!(
            p.socket().exists(),
            "marion unlinked a socket it had no proof was dead"
        );
        assert_eq!(
            std::fs::symlink_metadata(p.lock()).unwrap().ino(),
            inode_before,
            "the lock file was replaced, which would let two processes flock two inodes"
        );
        drop(held);
    }

    /// A supervisor that ends cleanly leaves no socket behind, so the next start does not have to
    /// reason about a corpse at all.
    #[test]
    fn a_server_that_drops_takes_its_socket_with_it_and_leaves_its_lock() {
        let dir = ShortDir::new("drop");
        let p = paths_in(&dir);
        {
            let Acquired::Serving(_s) = acquire(&p).unwrap() else {
                panic!("first")
            };
            assert!(p.socket().exists());
        }
        assert!(!p.socket().exists(), "a clean exit unlinks its socket");
        assert!(
            p.lock().exists(),
            "the lock file persists on purpose: unlinking it would let two processes flock two \
             different inodes at one path and both conclude they are alone"
        );
        // And the lock is free again, because the kernel drops an `flock` with its holder — asserted
        // through `acquire` rather than through `take_lock`, because the *release* is not
        // synchronous (see [`leave_a_corpse`]) and the marion-level claim is the one that matters:
        // a supervisor that ended cleanly leaves a project another supervisor can start in.
        let Acquired::Serving(_next) = acquire(&p).unwrap() else {
            panic!("the previous supervisor is gone; this one must be able to serve")
        };
    }

    /// The `/tmp` branch is audited, because `/tmp` is writable by everyone and the socket is the
    /// fleet's control plane. A directory somebody else owns is refused with a sentence rather
    /// than served through.
    #[test]
    fn a_tmp_fallback_directory_owned_by_someone_else_is_refused() {
        let dir = ShortDir::new("tmpguard");
        let target = dir.join("marion-99999");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o777)
            .create(&target)
            .unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o777)).unwrap();
        let p = SocketPaths {
            socket: target.join("a.sock"),
            lock: target.join("a.lock"),
            dir: target.clone(),
            overflow: Some("pretend the state path overflowed".into()),
        };
        let e = acquire(&p).unwrap_err();
        let SocketError::UnsafeDir { detail, .. } = &e else {
            panic!("a world-writable socket directory must be refused: {e}")
        };
        assert!(detail.contains("write"), "{detail}");

        // The same directory, private, is fine — so the check is about the mode and not about
        // being in the fallback branch at all.
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(acquire(&p).unwrap(), Acquired::Serving(_)));
    }

    /// A fallback directory that does not exist yet is created private, not merely created.
    #[test]
    fn a_created_socket_directory_is_private_and_so_is_the_socket() {
        let dir = ShortDir::new("mode");
        let target = dir.join("nested/deeper");
        let p = paths_in(&target);
        let Acquired::Serving(_s) = acquire(&p).unwrap() else {
            panic!("serve")
        };
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(p.socket()).unwrap().permissions().mode() & 0o777,
            0o600,
            "the socket is the fleet's control plane; §5.4's model is that the client is the \
             operator, and the file mode is what says which operator"
        );
    }
}
