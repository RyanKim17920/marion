//! **§2's `agent/spawn`, against a supervisor that is a real detached process** (§11 item 28
//! step 5).
//!
//! `handler.rs`'s own test module drives `RegistryHandle::call` directly and can therefore claim a
//! node and hold its token, which is what every assertion about a *served* spawn needs. What it
//! cannot show is the thing this file exists for: that the process `detach.rs` stage 3 actually
//! starts is built by `RegistryHandle::owning` and not by `RegistryHandle::new`, and that the two
//! values `owning` needs and cannot derive — the auth mode and the endpoint — survive the double
//! fork.
//!
//! # Why nothing here spawns a node, and what stands in for it
//!
//! A served spawn needs a `SpawnCaller` whose token **this** supervisor minted, and today nothing
//! can put a first node in a detached supervisor's table: `claim` is reached only from the
//! supervisor's own `agent/spawn`, and the only spawn that needs no caller is a root, which is §11
//! item 28 step 6 and is refused. So the bootstrap is circular until step 6 lands, and no test in
//! this file can break the circle without a back door that would assert against a binding
//! production does not make.
//!
//! What is observable across the process boundary is **which refusal comes back**, and that is
//! enough to pin the change: a stage 3 built by `new` answers a well-formed root spawn by naming
//! the constructor, and a stage 3 built by `owning` answers the same frame by naming step 6. The
//! two sentences are disjoint, so the assertion below fails the moment `owning` is reverted to
//! `new` — which is exactly the mutation this file is here to kill. When step 6 lands, this file is
//! where the served spawn belongs.

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
    paths: SocketPaths,
}

impl Bed {
    fn new(tag: &str) -> Bed {
        let state = PathBuf::from(format!("/tmp/mspawn-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        std::fs::create_dir_all(&state).expect("scratch state dir");
        let root = state.join("proj");
        std::fs::create_dir_all(&root).expect("project root");
        // SAFETY: reads the calling process's real uid and cannot fail.
        let paths = socket_paths(&state, &root, unsafe { getuid() });
        assert!(paths.socket().as_os_str().len() <= 103);
        Bed { state, root, paths }
    }

    fn launch(&self) -> Launch {
        Launch {
            program: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
            state_dir: self.state.clone(),
            project_root: self.root.clone(),
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
        caller: None,
        repo: repo.map(Path::to_path_buf),
        acceptance_criteria: vec![],
        writable_scope: vec![],
        timeout_secs: Some(1),
        model: None,
        no_change_record: None,
    }
}

/// **The detached supervisor owns nodes: it is built by `owning`, not by `new`.**
///
/// Read the module doc for why the assertion is on which refusal comes back rather than on a node
/// that started. The two sentences are disjoint by construction — one names
/// `RegistryHandle::new`, the other names step 6 — so this fails against a stage 3 that builds a
/// describing handle, and it goes on failing however that regression is spelled.
///
/// The frame is the **well-formed** root shape. A root spawn missing its `repo` is refused one step
/// earlier, by a check that does not consult the environment at all, and would pass this test
/// against either constructor.
#[test]
fn a_detached_supervisor_answers_agent_spawn_from_a_spawn_environment_it_was_given() {
    let bed = Bed::new("has-env");
    let ensured = ensure_supervisor(&bed.paths, &bed.launch()).expect("a supervisor starts");

    let e = agent_spawn(&bed.paths, root_spawn(Some(&bed.root)))
        .expect_err("root creation is step 6 and is not served yet");

    assert!(
        !e.message.contains("RegistryHandle::new"),
        "this supervisor must not be a describing handle — that refusal means stage 3 built \
         `RegistryHandle::new` and no socket `agent/spawn` can ever be served: {}",
        e.message
    );
    assert_eq!(e.kind(), Some(FailureKind::Unimplemented), "{e:?}");
    assert!(
        e.message.contains("step 6"),
        "the only thing left owing is root creation, and the refusal must say so: {}",
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
