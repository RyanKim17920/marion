//! **§2's `agent/spawn`, against a supervisor that is a real detached process** (§11 item 28
//! steps 4-6).
//!
//! `handler.rs`'s own test module drives `RegistryHandle::call` directly and can therefore claim a
//! node and hold its token, which is what every assertion about a *served* spawn needs. What it
//! cannot show is the thing this file exists for: that the process `detach.rs` stage 3 actually
//! starts is built by `RegistryHandle::owning` and not by `RegistryHandle::new`, and that the two
//! values `owning` needs and cannot derive — the auth mode and the endpoint — survive the double
//! fork.
//!
//! # Why nothing here launches a node, and what stands in for it
//!
//! Step 6 broke the bootstrap this file was written under: a root needs no caller, so a detached
//! supervisor can now be given its first node over this socket and no back door is needed. What
//! this file still does not do is *launch* one, because launching a root means a real harness
//! binary, a real working tree and a provider — which is `tests/client_run.rs`'s bed, driven
//! through `marion run` the way an operator drives it.
//!
//! The *child* half of this method is exercised end to end by `tests/background_spawn.rs`, where a
//! real bridge presents a real capability token over this same socket (step 5). What this file adds
//! is the argument about the **constructor**, which no bridge can make.
//!
//! What is observable here without any of that is **which refusal comes back**, and it is still
//! enough to pin the constructor: a stage 3 built by `RegistryHandle::new` refuses every
//! `agent/spawn` by naming the constructor, before it looks at anything in the frame; a stage 3
//! built by `owning` gets as far as resolving the agent type and refuses an unknown one by naming
//! *that*. The two sentences are disjoint, so the assertion below fails the moment `owning` is
//! reverted to `new`.

use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use marion_proto::params::AgentSpawnParams;
use marion_proto::{Call, FailureKind, Frame, Outcome, Request, RequestId, RpcError, SpawnCaller};
use marion_supervisor::detach::{Launch, ensure_supervisor};
use marion_supervisor::socket::{SocketPaths, socket_paths};

unsafe extern "C" {
    fn getuid() -> u32;
    fn kill(pid: i32, sig: i32) -> i32;
}

/// §5.7's grace, shrunk: nothing here waits it out, and a supervisor left behind by a failing
/// assertion should go on its own rather than outlive the suite.
const GRACE: Duration = Duration::from_millis(300);

/// A scratch state directory with a **short** path, for `socket.rs`'s reason: a socket under
/// macOS's `/private/var/folders/…` temp dir overruns the 103 bytes a `sun_path` may hold.
struct Bed {
    state: PathBuf,
    root: PathBuf,
    /// **The project key, resolved the way production resolves it.**
    ///
    /// [`Launch::project_root`] is documented as *"already resolved by
    /// `crate::socket::project_root`"*, and `marion.rs` honours that — it keys the socket, the
    /// `Launch` and the `ProjectDir` off one `socket::project_root(&repo)`. This bed used to pass
    /// [`Self::root`] raw, which differs from the resolved key by exactly a `canonicalize`: on
    /// macOS `/tmp` is a symlink to `/private/tmp`, so the two hash differently and the supervisor
    /// came up keyed on a path no client would ever name. Nothing noticed while no code compared
    /// the two; `handler::spawn_root`'s repository check does, and it is right to.
    key: PathBuf,
    paths: SocketPaths,
}

impl Bed {
    fn new(tag: &str) -> Bed {
        Bed::build(tag, false)
    }

    /// A bed whose project root is a **real repository**, for the one test that needs a linked
    /// worktree — which is the case §2's keying exists for and the case the repository check must
    /// not break.
    fn new_repo(tag: &str) -> Bed {
        Bed::build(tag, true)
    }

    fn build(tag: &str, repo: bool) -> Bed {
        let state = PathBuf::from(format!("/tmp/mspawn-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        std::fs::create_dir_all(&state).expect("scratch state dir");
        let root = state.join("proj");
        std::fs::create_dir_all(&root).expect("project root");
        if repo {
            init_repo(&root);
        }
        // **After the `git init`, deliberately.** The key of a directory changes the moment it
        // becomes a repository — `project_root` starts answering the common dir — so resolving it
        // first would key the supervisor on something no client could name.
        let key = marion_supervisor::socket::project_root(&root);
        // SAFETY: reads the calling process's real uid and cannot fail.
        let paths = socket_paths(&state, &key, unsafe { getuid() });
        assert!(paths.socket().as_os_str().len() <= 103);
        Bed {
            state,
            root,
            key,
            paths,
        }
    }

    fn launch(&self) -> Launch {
        Launch {
            program: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
            state_dir: self.state.clone(),
            project_root: self.key.clone(),
            idle_grace: GRACE,
            auth: marion_harness::Auth::Canned,
            base_url: Some("http://127.0.0.1:8099/v1".into()),
        }
    }

    /// Every process whose argv names this bed's state directory, **whichever stage it is**. The
    /// path appears verbatim in all three stages' argv, so this names exactly the processes this
    /// test started and no others.
    fn supervisors(&self) -> Vec<i32> {
        let out = Command::new("ps").args(["-Ao", "pid=,args="]).output();
        let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        text.unwrap_or_default()
            .lines()
            .filter(|l| l.contains(&self.state.display().to_string()))
            .filter_map(|l| l.split_whitespace().next()?.parse().ok())
            .collect()
    }

    /// §5.7: zero clients and zero non-terminal nodes, so it must leave on its own.
    ///
    /// Waited on **before** the directory is removed, and that ordering is the whole point: a
    /// supervisor still running when its project directory goes away recreates enough of it to
    /// journal into, so removing first leaves a directory behind and hides the fact that the
    /// process outlived the test.
    fn goes_on_its_own(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !self.supervisors().is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            self.supervisors().is_empty(),
            "§5.7: this supervisor must leave without being killed by the fixture, and {:?} did not",
            self.supervisors()
        );
    }
}

impl Drop for Bed {
    /// Leave neither a supervisor nor a directory behind, whatever the test did or failed to do.
    /// The sweep is a safety net for a *failing* test; every passing test above asserts
    /// [`Bed::goes_on_its_own`] first, because a fixture that is the only thing ending a supervisor
    /// is a leak the assertions cannot see.
    fn drop(&mut self) {
        for pid in self.supervisors() {
            // SAFETY: `kill` with a pid this process just read from `ps`.
            unsafe { kill(pid, 9) };
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !self.supervisors().is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = std::fs::remove_dir_all(&self.state);
    }
}

/// `git`, run in `dir`, insisting it worked — a silent `git` failure here would turn a worktree
/// test into a two-plain-directories test that passes for the wrong reason.
fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} in {}: {e}", dir.display()));
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A one-commit repository, with the identity and default branch pinned so the test does not
/// depend on the machine's `git config`.
fn init_repo(dir: &Path) {
    git(dir, &["init", "--initial-branch=main"]);
    git(dir, &["config", "user.email", "bed@marion.test"]);
    git(dir, &["config", "user.name", "bed"]);
    std::fs::write(dir.join("README"), b"bed\n").expect("a file to commit");
    git(dir, &["add", "README"]);
    git(dir, &["commit", "-m", "bed"]);
}

/// Send one `agent/spawn` to a running supervisor and hand back what it answered.
fn agent_spawn(paths: &SocketPaths, p: AgentSpawnParams) -> Result<serde_json::Value, RpcError> {
    let mut c = UnixStream::connect(paths.socket()).expect("dial the supervisor");
    c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let frame = Frame::Request(Request::new(RequestId::Number(1), Call::AgentSpawn(p)));
    c.write_all(frame.to_line().as_bytes()).unwrap();
    c.flush().unwrap();
    let mut line = String::new();
    let mut r = std::io::BufReader::new(c.try_clone().unwrap());
    assert!(
        r.read_line(&mut line).expect("a frame arrives") > 0,
        "closed"
    );
    let Frame::Response(resp) = Frame::from_line(&line).expect("well-formed") else {
        panic!("a request is answered by a response: {line}")
    };
    match resp.outcome {
        Outcome::Result(v) => Ok(v),
        Outcome::Error(e) => Err(e),
    }
}

fn root_spawn(repo: Option<&Path>) -> AgentSpawnParams {
    AgentSpawnParams {
        agent_type: "claude".into(),
        prompt: "unused: every case here is refused before anything launches".into(),
        native_launch: None,
        caller: None,
        repo: repo.map(Path::to_path_buf),
        acceptance_criteria: vec![],
        writable_scope: vec![],
        timeout_secs: Some(1),
        model: None,
        no_change_record: None,
        pane: None,
        // A root: `isolation` and `allow_concurrent_writes` are child-only and refused
        // beside `caller: None` (§6.6, §9).
        isolation: None,
        allow_concurrent_writes: None,
    }
}

/// **The detached supervisor owns nodes: it is built by `owning`, not by `new` — and root creation
/// is served rather than refused.**
///
/// Renamed and re-aimed at §11 item 28 step 6. It used to be
/// `a_detached_supervisor_answers_agent_spawn_from_a_spawn_environment_it_was_given`, and it
/// asserted that a well-formed root spawn came back refused *by name*, with `step 6` in the
/// sentence — which was the honest pin while a client could not create a root. That is now false by
/// design, so the test asserts the two things that replaced it: the refusal is not the describing
/// handle's, and it is not step 6's either.
///
/// **The agent type is deliberately unknown**, and that is what keeps this file from needing a
/// harness. A root spawn that names a real type would launch a real process here; one that names no
/// type at all reaches `spawn_root`, is refused by the same lookup `root::prepare` performs, and
/// journals nothing — so the frame travels the whole served path without a node ever existing.
///
/// Read the module doc for why the assertion is on which refusal comes back. The three sentences
/// are disjoint by construction — one names `RegistryHandle::new`, one names the agent type, one
/// named step 6 — so this fails against a stage 3 that builds a describing handle *and* against one
/// that goes back to refusing roots.
#[test]
fn a_detached_supervisor_serves_root_creation_rather_than_naming_a_step_that_would() {
    let bed = Bed::new("has-env");
    let ensured = ensure_supervisor(&bed.paths, &bed.launch()).expect("a supervisor starts");

    let e = agent_spawn(
        &bed.paths,
        AgentSpawnParams {
            native_launch: None,
            agent_type: "no-such-agent-type".into(),
            ..root_spawn(Some(&bed.root))
        },
    )
    .expect_err("no build has that agent type");

    assert!(
        !e.message.contains("RegistryHandle::new"),
        "this supervisor must not be a describing handle — that refusal means stage 3 built \
         `RegistryHandle::new` and no socket `agent/spawn` can ever be served: {}",
        e.message
    );
    assert!(
        !e.message.contains("step 6"),
        "root creation is served now; a build that still names the step that would serve it has \
         reverted: {}",
        e.message
    );
    assert_eq!(e.kind(), Some(FailureKind::Refused), "{e:?}");
    assert!(
        e.message
            .contains("the agent type is not one this build has"),
        "the frame reached the root launcher and was refused on its own merits: {}",
        e.message
    );
    drop(ensured);
    bed.goes_on_its_own();
}

/// **The `caller`/`repo` pairing is answered from the frame alone, across the socket.**
///
/// Both halves, against a real supervisor, because both are things a client can get wrong and
/// neither may be silently repaired — §11 item 23's rule, on the surface a client actually reaches.
///
/// The caller in the second half is a **forgery**, deliberately: the pairing is a property of the
/// frame and is checked before any state is consulted, so a supervisor that answered "unknown
/// token" here would be one where a client's malformed frame is diagnosed as an authorization
/// failure — and where the pairing rule is unreachable for every caller that does not already hold
/// a token.
#[test]
fn the_caller_and_repo_pairing_is_refused_by_name_over_the_socket() {
    let bed = Bed::new("pairing");
    let ensured = ensure_supervisor(&bed.paths, &bed.launch()).expect("a supervisor starts");

    let e = agent_spawn(&bed.paths, root_spawn(None))
        .expect_err("a root spawn must say which tree it is of");
    assert_eq!(e.kind(), Some(FailureKind::Refused), "{e:?}");
    assert!(
        e.message.contains("only the client knows which tree"),
        "{}",
        e.message
    );

    let e = agent_spawn(
        &bed.paths,
        AgentSpawnParams {
            native_launch: None,
            caller: Some(SpawnCaller {
                agent_id: marion_core::contract::AgentId(
                    "0199c0ff-ee00-7000-8000-000000000001".into(),
                ),
                node_token: "not-a-token".into(),
            }),
            ..root_spawn(Some(&bed.root))
        },
    )
    .expect_err("a caller may not state its own repository");
    assert_eq!(e.kind(), Some(FailureKind::Refused), "{e:?}");
    assert!(
        e.message.contains("must not state a `repo`"),
        "the frame is malformed and that is what must be reported, not the token: {}",
        e.message
    );

    drop(ensured);
    bed.goes_on_its_own();
}

/// The nodes a project's journal records, and the agent directories it has on disk — the two
/// places a root that was accepted would show up.
fn footprint(state: &Path, key: &Path) -> (usize, Vec<String>) {
    let project = marion_core::paths::ProjectDir::new(state, key);
    let bytes = std::fs::read(project.journal()).unwrap_or_default();
    let nodes = marion_core::registry::replay(&bytes).nodes().len();
    let dirs = std::fs::read_dir(project.agents_dir())
        .map(|es| {
            es.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    (nodes, dirs)
}

/// **A root is created only in the repository the socket serves** — and a linked worktree of that
/// repository still counts as it.
///
/// # The hole
///
/// [`root_spawn_authorized`] answers *who is calling* — a same-uid peer — and deliberately nothing
/// about *what they named*. Nothing else compared `p.repo` to the supervisor's own project key, so
/// a same-uid process could dial repository A's socket, send `agent/spawn` with `caller: None` and
/// `repo: B`, and be obeyed: `root::prepare_watched` keys the agent directory and the `SpawnIntent`
/// on `ProjectDir::new(state, project_root(&spec.repo))`, so the node landed in **B's** journal
/// while the supervisor that answered went on following **A's**. marion replied "spawned" for a
/// root that no `tree/subscribe` on that socket could ever list. That is an authorization failure
/// and a broken contract at once, which is why the refusal is checked before `NodeOwner::claim` and
/// before any side effect.
///
/// # Why the second half is not optional
///
/// §2 keys on the git **common dir**, so one supervisor serves a repository *and every linked
/// worktree of it* — W1's whole "repo is a property of the tree" result depends on a worktree root
/// being accepted. A check written on the path rather than the key would pass the first half of
/// this test and silently break that, so the worktree is spawned here too and must get **past** the
/// repository check. It is proved to have got past by the refusal it does come back with: an
/// unknown agent type, which is resolved strictly after. Nothing launches either way, so this file
/// still needs no harness.
#[test]
fn a_root_for_another_repository_is_refused_on_the_socket_that_does_not_serve_it() {
    let bed = Bed::new_repo("wrong-repo");
    let other = bed.state.join("other-repo");
    std::fs::create_dir_all(&other).expect("a second repository");
    init_repo(&other);
    let other_key = marion_supervisor::socket::project_root(&other);
    assert_ne!(
        other_key, bed.key,
        "the premise: two repositories, two keys — otherwise this test is about nothing"
    );

    let ensured = ensure_supervisor(&bed.paths, &bed.launch()).expect("a supervisor starts");

    // **A real agent type**, so that with the check removed this frame does not stop at a lookup:
    // it reaches `root::prepare_watched`, which creates the agent directory and journals the
    // intent under `other` before any harness binary is needed. That is what makes the footprint
    // assertions below a measurement rather than a restatement of the refusal.
    let e = agent_spawn(&bed.paths, root_spawn(Some(&other)))
        .expect_err("this socket does not serve that repository");
    assert_eq!(e.kind(), Some(FailureKind::Refused), "{e:?}");
    // **Both projects, by key, and the repository by path.** The supervisor cannot name A's
    // checkout — `RegistryHandle` deliberately holds no repository, because one serves a repository
    // and all its worktrees — so the two things it can honestly name are the key it serves and the
    // key the named repository resolves to. An operator with two checkouts open needs all three to
    // see which socket they reached.
    let mine = marion_core::paths::ProjectDir::new(&bed.state, &bed.key);
    let theirs = marion_core::paths::ProjectDir::new(&bed.state, &other_key);
    for needle in [
        mine.path().display().to_string(),
        theirs.path().display().to_string(),
        other.display().to_string(),
    ] {
        assert!(
            e.message.contains(&needle),
            "the refusal must name {needle}, so the operator can tell which socket they reached: \
             {}",
            e.message
        );
    }

    // **Nothing anywhere.** Refused before the claim, so neither project gained a record or a
    // directory — the failure this guards is a node journaled under `other` and invisible here.
    assert_eq!(
        footprint(&bed.state, &other_key),
        (0, vec![]),
        "the repository that was named must be untouched: a journal record or an agent directory \
         under it is precisely the node marion would have reported and never been able to show"
    );
    assert_eq!(
        footprint(&bed.state, &bed.key),
        (0, vec![]),
        "and the project this socket does serve gained nothing either — the refusal is not a \
         mis-filing, it is a refusal"
    );

    // **A linked worktree of this repository is a different matter**: same common dir, same key,
    // so it must be served. It gets as far as the agent-type lookup, which is after the check.
    let wt = bed.state.join("wt");
    git(&bed.root, &["worktree", "add", "-b", "feature", "../wt"]);
    assert_eq!(
        marion_supervisor::socket::project_root(&wt),
        bed.key,
        "§2 keys on the git common dir, so a linked worktree of this repository keys to it"
    );
    let e = agent_spawn(
        &bed.paths,
        AgentSpawnParams {
            native_launch: None,
            agent_type: "no-such-agent-type".into(),
            ..root_spawn(Some(&wt))
        },
    )
    .expect_err("no build has that agent type");
    assert!(
        e.message
            .contains("the agent type is not one this build has"),
        "a linked worktree root must reach its own merits — a repository refusal here is the \
         check written on the path instead of the key, which would undo W1: {}",
        e.message
    );

    // §5.7, and the sharpest process assertion available: a supervisor holding a claimed node
    // cannot leave, so this also says neither frame left one behind.
    drop(ensured);
    bed.goes_on_its_own();
}

/// **An absent auth mode is a hard error, never a silent `Canned`.**
///
/// This is the whole reason the mode rides argv. `main::auth_from_env`'s rule — an absent
/// `MARION_AUTH` means `Canned` — is right where it lives, because every declaration written
/// before the key existed meant canned. Applied to a *supervisor*, it means a stage 3 whose
/// launcher's environment did not survive the double fork runs an operator's live fleet against a
/// canned endpoint nobody is listening on, and reports nothing.
///
/// Driven through the real binary rather than through `parse_serve`, because the property is about
/// what the process does: it must **not start**. A parse that returned a default would leave a
/// supervisor serving with the wrong mode, and only a process can show that it did not.
///
/// The pair is checked in both directions for the same reason: a `--base-url` under `inherited`
/// is a value stage 3 would have to ignore, which is a flag that says one thing and does another.
///
/// **The tiny idle grace is what keeps the failure a failure rather than a hang.** An argv these
/// assertions reject is one that binds nothing and exits at once; an argv a regression *accepts*
/// becomes a supervisor, and a supervisor with §5.7's real five-minute grace would leave this call
/// blocked on a process that is behaving perfectly. Fifty milliseconds makes that case exit 0
/// instead, which is what the first assertion reads.
#[test]
fn a_supervisor_argv_that_does_not_state_its_auth_mode_will_not_start() {
    let bed = Bed::new("no-auth");
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor"));
    let common = [
        "serve".to_string(),
        "--state-dir".to_string(),
        bed.state.display().to_string(),
        "--project-root".to_string(),
        bed.root.display().to_string(),
        "--idle-grace-ms".to_string(),
        "50".to_string(),
        "--detached".to_string(),
    ];
    for (extra, why) in [
        (vec![], "no auth mode at all — the silent-`Canned` case"),
        (
            vec!["--auth".to_string(), "canned".to_string()],
            "canned with no endpoint: marion names the endpoint in this mode, so there is nothing \
             to fall back to that is not an invented default",
        ),
        (
            vec![
                "--auth".to_string(),
                "inherited".to_string(),
                "--base-url".to_string(),
                "http://127.0.0.1:8099/v1".to_string(),
            ],
            "an endpoint under a mode that overlays none — a flag stage 3 would have to ignore",
        ),
        (
            vec!["--auth".to_string(), "live".to_string()],
            "a spelling `Auth::from_wire` does not know, which must not decay to a mode",
        ),
    ] {
        let out = Command::new(&bin)
            .args(common.iter().chain(extra.iter()))
            .output()
            .expect("the supervisor binary runs");
        assert!(
            !out.status.success(),
            "{why}: this argv must not produce a supervisor, and it exited 0"
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("--auth"),
            "{why}: the refusal must name what was missing or wrong: {err}"
        );
        assert!(
            !bed.paths.socket().exists(),
            "{why}: nothing may have bound the socket"
        );
    }
}
