//! Tests for `root.rs`, moved out of it unchanged.

use super::*;
use marion_core::agent_type::builtin;
use marion_harness::claude_code::mcp_config_json;
use marion_harness::spec::base_url_root as anthropic_base_url;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::json;

struct RecordingPaneOwner {
    host: usize,
    events: Mutex<Vec<&'static str>>,
}

impl RecordingPaneOwner {
    fn record(&self, host: &Arc<crate::pty::PtyHost>, event: &'static str) {
        assert_eq!(Arc::as_ptr(host) as usize, self.host);
        self.events
            .lock()
            .expect("pane lifecycle events")
            .push(event);
    }
}

impl PaneOwner for RecordingPaneOwner {
    fn opened(&self, _: &AgentId, _: Arc<crate::pty::PtyHost>) {
        panic!("the finish seam must not re-open a pane");
    }

    fn closing(&self, _: &AgentId, host: &Arc<crate::pty::PtyHost>) {
        self.record(host, "closing");
    }

    fn completed(&self, _: &AgentId, host: &Arc<crate::pty::PtyHost>, charge: usize) {
        assert!(
            charge > 0,
            "a completed replay has a nonzero retained charge"
        );
        self.record(host, "completed");
    }

    fn failed(&self, _: &AgentId, _: &Arc<crate::pty::PtyHost>) {
        panic!("an eligible pane must not be failed");
    }
}

struct FailingPaneOwner {
    host: usize,
    events: Mutex<Vec<&'static str>>,
}

impl FailingPaneOwner {
    fn record(&self, host: &Arc<crate::pty::PtyHost>, event: &'static str) {
        assert_eq!(Arc::as_ptr(host) as usize, self.host);
        self.events
            .lock()
            .expect("pane lifecycle events")
            .push(event);
    }
}

impl PaneOwner for FailingPaneOwner {
    fn opened(&self, _: &AgentId, _: Arc<crate::pty::PtyHost>) {
        panic!("the finish seam must not re-open a pane");
    }

    fn closing(&self, _: &AgentId, host: &Arc<crate::pty::PtyHost>) {
        self.record(host, "closing");
    }

    fn completed(&self, _: &AgentId, _: &Arc<crate::pty::PtyHost>, _: usize) {
        panic!("a failed or ineligible pane must never be completed");
    }

    fn failed(&self, _: &AgentId, host: &Arc<crate::pty::PtyHost>) {
        self.record(host, "failed");
    }
}

/// Root exit polling must observe without reaping: shutdown needs the waitable leader to pin
/// its process-group identity while it sweeps a same-group slave holder. This fixture calls
/// the production wait and finish helpers; a direct `PtyHost::shutdown` test cannot catch a
/// consuming root poll.
#[test]
fn root_wait_keeps_the_leader_waitable_until_the_same_group_sweep() {
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct KillHolderOnDrop {
        pid_file: PathBuf,
        armed: bool,
    }

    impl Drop for KillHolderOnDrop {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            let Ok(raw) = std::fs::read_to_string(&self.pid_file) else {
                return;
            };
            let Ok(pid) = raw.parse::<i32>() else {
                return;
            };
            if let Some(pid) =
                rustix::process::Pid::from_raw(pid).filter(|pid| *pid != rustix::process::Pid::INIT)
            {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
    }

    fn alive(pid: i32) -> bool {
        let Some(pid) = rustix::process::Pid::from_raw(pid) else {
            return false;
        };
        match rustix::process::test_kill_process(pid) {
            Ok(()) => true,
            Err(rustix::io::Errno::SRCH) => false,
            Err(_) => true,
        }
    }

    let dir = marion_testsupport::scratch("root-wait-unreaped-pgid-holder");
    let holder_file = dir.join("holder.pid");
    let ready_file = dir.join("holder.ready");
    let id = AgentId("root-wait-unreaped-pgid-holder".into());
    let host = Arc::new(
        crate::pty::PtyHost::start(
            id.clone(),
            crate::pty::PtyMaster::open(PANE_SIZE).expect("open pty"),
            &dir.join("pty.cast"),
            PANE_SIZE,
            PANE_TERM,
            std::time::Instant::now(),
        )
        .expect("start pane host"),
    );
    let witness = marion_harness::ExecutionSurfaces::opaque()
        .display_plane()
        .expect("opaque execution owns a pty");
    let mut command = SysCommand::new("/bin/sh");
    command
        .arg("-c")
        .arg(
            "(trap '' HUP TERM; printf ready >\"$READY_FILE\"; exec sleep 30) & \
             holder=$!; printf '%s' \"$holder\" >\"$HOLDER_FILE\"; \
             while [ ! -s \"$READY_FILE\" ]; do :; done; \
             printf 'leader-tail'; exit 37",
        )
        .env("HOLDER_FILE", &holder_file)
        .env("READY_FILE", &ready_file);
    host.adopt(
        crate::pty::spawn_pty(
            witness,
            &mut command,
            host.master(),
            crate::pty::StdinPlan::TerminalSlave,
            None,
        )
        .expect("spawn fast leader and same-group holder"),
    );
    let mut cleanup = KillHolderOnDrop {
        pid_file: holder_file.clone(),
        armed: true,
    };

    let marker_deadline = std::time::Instant::now() + StdDuration::from_secs(2);
    let holder_pid = loop {
        if let Ok(file) = std::fs::File::open(&holder_file) {
            let mut raw = String::new();
            file.take(64)
                .read_to_string(&mut raw)
                .expect("read holder pid");
            if let Ok(pid) = raw.parse::<i32>() {
                break pid;
            }
        }
        assert!(
            std::time::Instant::now() < marker_deadline,
            "the same-group holder never published its pid"
        );
        std::thread::yield_now();
    };
    let leader_pid = host.child_pid().expect("the leader is adopted");
    let leader = rustix::process::Pid::from_raw(leader_pid).expect("positive leader pid");
    let saw_waitable_leader = Arc::new(AtomicBool::new(false));
    let saw_waitable_leader_in_hook = Arc::clone(&saw_waitable_leader);
    host.set_before_child_sweep_hook(Box::new(move || {
        let observed = rustix::process::waitid(
            rustix::process::WaitId::Pid(leader),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::NOHANG
                | rustix::process::WaitIdOptions::NOWAIT,
        )
        .expect("root polling must leave the leader waitable until the sweep");
        assert!(
            observed.is_some(),
            "the sweep boundary must observe the exited leader as a waitable zombie"
        );
        saw_waitable_leader_in_hook.store(true, Ordering::SeqCst);
    }));

    let waited = wait_for_the_pane_to_end(&host, Some(StdDuration::from_secs(5)), &mut || false);
    let (status, timed_out) = finish_terminal_pane(
        None,
        &id,
        &host,
        waited,
        |timed_out| host.shutdown_with_timeout_outcome(timed_out),
        || unreachable!("an unadvertised test pane requests no replay charge"),
    )
    .expect("root wait and shutdown succeed");
    assert!(!timed_out);
    assert_eq!(status.and_then(|status| status.code()), Some(37));
    assert!(
        saw_waitable_leader.load(Ordering::SeqCst),
        "shutdown reached the pre-sweep waitability assertion"
    );
    assert!(
        matches!(
            rustix::process::waitid(
                rustix::process::WaitId::Pid(leader),
                rustix::process::WaitIdOptions::EXITED
                    | rustix::process::WaitIdOptions::NOHANG
                    | rustix::process::WaitIdOptions::NOWAIT,
            ),
            Err(rustix::io::Errno::CHILD)
        ),
        "the sole shutdown wait must consume the leader status"
    );
    let holder_deadline = std::time::Instant::now() + StdDuration::from_secs(2);
    while alive(holder_pid) && std::time::Instant::now() < holder_deadline {
        std::thread::yield_now();
    }
    assert!(
        !alive(holder_pid),
        "the same-group holder survived the sweep"
    );
    cleanup.armed = false;
}

/// Mutation: hard-code `timed_out: false` while sealing the authoritative PTY stream. The
/// root outcome can still report the wait timeout correctly, so only recovering the durable
/// terminal End proves that the recorded lifecycle agrees with the root result.
#[test]
fn bounded_terminal_timeout_is_recorded_in_the_recovered_session_end() {
    use std::os::unix::process::ExitStatusExt;

    let dir = marion_testsupport::scratch("root-pane-timeout-outcome");
    let cast = dir.join("pty.cast");
    let id = AgentId("root-pane-timeout-outcome".into());
    let host = Arc::new(
        crate::pty::PtyHost::start(
            id.clone(),
            crate::pty::PtyMaster::open(PANE_SIZE).expect("open pty"),
            &cast,
            PANE_SIZE,
            PANE_TERM,
            std::time::Instant::now(),
        )
        .expect("start pane host"),
    );
    let witness = marion_harness::ExecutionSurfaces::opaque()
        .display_plane()
        .expect("opaque execution owns a pty");
    let mut command = SysCommand::new("/bin/sleep");
    command.arg("30");
    host.adopt(
        crate::pty::spawn_pty(
            witness,
            &mut command,
            host.master(),
            crate::pty::StdinPlan::TerminalSlave,
            None,
        )
        .expect("spawn bounded pane"),
    );

    let waited = wait_for_the_pane_to_end(&host, Some(StdDuration::from_millis(25)), &mut || false);
    let (status, timed_out) = finish_terminal_pane(
        None,
        &id,
        &host,
        waited,
        |timed_out| host.shutdown_with_timeout_outcome(timed_out),
        || unreachable!("an unadvertised test pane requests no replay charge"),
    )
    .expect("timed-out pane is killed and reaped");
    let status = status.expect("the adopted child has an exact kill status");
    assert_eq!(status.code(), None);
    assert_eq!(status.signal(), Some(9));
    assert!(timed_out);

    let stream_path = crate::pty::stream::stream_path_for_cast(&cast).expect("stream path");
    let encoded = std::fs::read(stream_path).expect("sealed authoritative stream");
    let recovered =
        crate::pty::stream::recover_session_bytes(&encoded).expect("the sealed stream recovers");
    let crate::pty::stream::RecordKind::End(outcome) = recovered
        .records
        .last()
        .expect("the session has a terminal record")
        .kind
    else {
        panic!("the final authoritative record is a typed End")
    };
    assert_eq!(outcome.exit_code, None);
    assert_eq!(outcome.signal, Some(9));
    assert!(outcome.timed_out);
}

#[test]
fn terminal_pane_completes_only_after_shutdown_and_replay_proof() {
    let id = AgentId("root-pane-finish".into());
    // A private scratch directory, like every other pane test here, and not `temp_dir()`:
    // marion's own replay check refuses a stream whose parent is not private to the effective
    // user, and on Linux `temp_dir()` is `/tmp` at mode 1777. The refusal was correct; the
    // fixture was putting the cast somewhere the production code is right to distrust.
    let dir = marion_testsupport::scratch("root-pane-finish");
    let cast = dir.join("pty.cast");
    let host = Arc::new(
        crate::pty::PtyHost::start(
            id.clone(),
            crate::pty::PtyMaster::open(PANE_SIZE).expect("open pty"),
            &cast,
            PANE_SIZE,
            PANE_TERM,
            std::time::Instant::now(),
        )
        .expect("start pane host"),
    );
    let witness = marion_harness::ExecutionSurfaces::opaque()
        .display_plane()
        .expect("opaque execution owns a pty");
    let mut command = SysCommand::new("/bin/sh");
    command.arg("-c").arg("printf 'fast-tail'; exit 37");
    host.adopt(
        crate::pty::spawn_pty(
            witness,
            &mut command,
            host.master(),
            crate::pty::StdinPlan::TerminalSlave,
            None,
        )
        .expect("spawn fast pane"),
    );
    let deadline = std::time::Instant::now() + StdDuration::from_secs(5);
    while !host
        .poll_exited_unreaped()
        .expect("non-consuming exit poll")
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the fast pane did not exit within the bounded fixture"
        );
        std::thread::yield_now();
    }
    let owner = RecordingPaneOwner {
        host: Arc::as_ptr(&host) as usize,
        events: Mutex::new(Vec::new()),
    };

    let (status, timed_out) = finish_terminal_pane(
        Some(&owner),
        &id,
        &host,
        Ok(false),
        |timed_out| {
            owner
                .events
                .lock()
                .expect("pane lifecycle events")
                .push("shutdown");
            host.shutdown_with_timeout_outcome(timed_out)
        },
        || {
            owner
                .events
                .lock()
                .expect("pane lifecycle events")
                .push("replay-proof");
            host.completed_replay_charge()
        },
    )
    .expect("eligible pane finish");

    assert_eq!(status.and_then(|status| status.code()), Some(37));
    assert!(!timed_out);
    assert_eq!(
        *owner.events.lock().expect("pane lifecycle events"),
        ["closing", "shutdown", "replay-proof", "completed"]
    );

    let lines = std::fs::read_to_string(&cast).expect("read cast after shutdown");
    let exit = lines.lines().last().expect("cast has an exit record");
    let exit: Value = serde_json::from_str(exit).expect("exit record is JSON");
    assert_eq!(exit[1], "x", "the completed callback follows the cast exit");
    assert_eq!(exit[2], "37", "the cast preserves the exact process status");

    let conn = crate::serve::ConnId(9_001);
    let (out, rx) = crate::serve::capture(conn);
    let descriptor = host
        .begin_pane_replay(conn, out)
        .expect("eligible completed replay");
    host.pane_ready(conn, &descriptor.token, descriptor.cut);
    let frames = rx
        .try_iter()
        .map(|line| {
            marion_core::proto::Frame::from_line(
                std::str::from_utf8(&line).expect("outbound replay is UTF-8"),
            )
            .expect("outbound replay frame")
        })
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    let mut ends = 0;
    for frame in frames {
        let marion_core::proto::Frame::Notification(note) = frame else {
            panic!("pane replay emitted a non-notification")
        };
        let marion_core::proto::Event::NodePaneFrame(frame) = note.event else {
            panic!("pane replay emitted another event")
        };
        match frame.frame {
            marion_core::proto::PaneFrameKindV1::Output { bytes } => {
                output.extend_from_slice(bytes.as_bytes());
            }
            marion_core::proto::PaneFrameKindV1::End {} => ends += 1,
            marion_core::proto::PaneFrameKindV1::Resize { .. } => {}
        }
    }
    assert_eq!(output, b"fast-tail");
    assert_eq!(ends, 1, "the late replay has one terminal End");
    drop(host);
    let _ = std::fs::remove_file(cast);
}

#[test]
fn terminal_pane_wait_shutdown_and_replay_failures_are_permanently_failed() {
    use std::os::unix::process::ExitStatusExt;

    let id = AgentId("root-pane-failures".into());
    let cast = std::env::temp_dir().join(format!(
        "marion-root-pane-failures-{}-{}.cast",
        std::process::id(),
        crate::run::unix_millis()
    ));
    let host = Arc::new(
        crate::pty::PtyHost::start(
            id.clone(),
            crate::pty::PtyMaster::open(PANE_SIZE).expect("open pty"),
            &cast,
            PANE_SIZE,
            PANE_TERM,
            std::time::Instant::now(),
        )
        .expect("start pane host"),
    );
    let owner = FailingPaneOwner {
        host: Arc::as_ptr(&host) as usize,
        events: Mutex::new(Vec::new()),
    };

    let error = finish_terminal_pane(
        Some(&owner),
        &id,
        &host,
        Err(std::io::Error::other("wait failed")),
        |_| {
            owner
                .events
                .lock()
                .expect("pane lifecycle events")
                .push("shutdown");
            Ok(None)
        },
        || -> std::io::Result<usize> { panic!("wait failure cannot publish replay") },
    )
    .expect_err("wait failure stays a root error");
    assert!(error.to_string().contains("wait failed"));
    assert_eq!(
        *owner.events.lock().expect("pane lifecycle events"),
        ["closing", "shutdown", "failed"]
    );

    owner.events.lock().expect("pane lifecycle events").clear();
    let error = finish_terminal_pane(
        Some(&owner),
        &id,
        &host,
        Ok(false),
        |_| {
            owner
                .events
                .lock()
                .expect("pane lifecycle events")
                .push("shutdown");
            Err(std::io::Error::other("shutdown failed"))
        },
        || -> std::io::Result<usize> { panic!("shutdown failure cannot publish replay") },
    )
    .expect_err("shutdown failure stays a root error");
    assert!(error.to_string().contains("shutdown failed"));
    assert_eq!(
        *owner.events.lock().expect("pane lifecycle events"),
        ["closing", "shutdown", "failed"]
    );

    owner.events.lock().expect("pane lifecycle events").clear();
    let (status, timed_out) = finish_terminal_pane(
        Some(&owner),
        &id,
        &host,
        Ok(true),
        |_| {
            owner
                .events
                .lock()
                .expect("pane lifecycle events")
                .push("shutdown");
            Ok(Some(std::process::ExitStatus::from_raw(37 << 8)))
        },
        || {
            owner
                .events
                .lock()
                .expect("pane lifecycle events")
                .push("replay-proof");
            Err(std::io::Error::other("replay ineligible"))
        },
    )
    .expect("replay ineligibility must not change core shutdown success");
    assert_eq!(status.and_then(|status| status.code()), Some(37));
    assert!(timed_out);
    assert_eq!(
        *owner.events.lock().expect("pane lifecycle events"),
        ["closing", "shutdown", "replay-proof", "failed"]
    );

    host.shutdown().expect("shut down fixture host");
    drop(host);
    let _ = std::fs::remove_file(cast);
}

fn env() -> BridgeEnv {
    BridgeEnv {
        node_token_file: None,
        bridge: "/bin/marion-supervisor".into(),
        args: vec!["mcp".into()],
        repo: "/repo".into(),
        state: "/state".into(),
        base_url: Some("http://127.0.0.1:8099/v1".into()),
        auth: Auth::Canned,
        agent_id: AgentId("019f-root".into()),
        agent_type: "claude".into(),
        depth: ROOT_DEPTH,
        node_token: None,
        ready_file: Some("/state/x/mcp-ready".into()),
    }
}

#[test]
fn claude_gets_the_base_url_without_the_v1_it_appends_itself() {
    assert_eq!(
        anthropic_base_url("http://127.0.0.1:8099/v1"),
        "http://127.0.0.1:8099",
        "leaving it on yields a request to /v1/v1/messages"
    );
    assert_eq!(
        anthropic_base_url("http://127.0.0.1:8099/v1/"),
        "http://127.0.0.1:8099"
    );
    assert_eq!(
        anthropic_base_url("http://127.0.0.1:8099"),
        "http://127.0.0.1:8099"
    );
}

#[test]
fn the_declaration_carries_the_root_agent_id_so_requester_is_not_a_placeholder() {
    let v = mcp_config_json(&env());
    assert_eq!(
        v["mcpServers"]["marion"]["env"][AGENT_ID_ENV], "019f-root",
        "§9: requester for a top-level spawn is the root's own AgentId"
    );
    assert_eq!(v["mcpServers"]["marion"]["args"][0], "mcp");
    assert_eq!(
        v["mcpServers"]["marion"]["env"]["MARION_BASE_URL"], "http://127.0.0.1:8099/v1",
        "the child's model_providers entry wants the /v1 form"
    );
}

#[test]
fn the_declaration_names_the_readiness_marker_the_prompt_waits_on() {
    let v = mcp_config_json(&env());
    assert_eq!(
        v["mcpServers"]["marion"]["env"][READY_FILE_ENV],
        "/state/x/mcp-ready"
    );
}

#[test]
fn the_root_allowlist_is_every_verb_an_m1_root_can_reach() {
    // Omitting a reachable verb denies a call that then blocks until the root's bound expires.
    // `steer` because a root may redirect its own descendants (§5.4), and the verb is declared.
    // `cancel` because it may stop them, on the same authority.
    assert_eq!(
        ROOT_VERBS.to_vec(),
        vec!["spawn", "status", "wait", "list", "steer", "cancel"]
    );
    assert!(
        !ROOT_VERBS.contains(&"report"),
        "report is rejected on a node without a contract, and a root has none"
    );
    // And on Claude Code the adapter spells each `mcp__marion__<verb>`, the strings a claude
    // root's `--allowedTools` carries.
    let claude = adapter_for(Harness::ClaudeCode).unwrap();
    assert_eq!(
        ROOT_VERBS
            .iter()
            .map(|v| claude.marion_tool_name(v))
            .collect::<Vec<_>>(),
        vec![
            "mcp__marion__spawn",
            "mcp__marion__status",
            "mcp__marion__wait",
            "mcp__marion__list",
            "mcp__marion__steer",
            "mcp__marion__cancel"
        ]
    );
    // Whereas copilot, the other adapter that compiles this list, spells them its own way.
    assert_eq!(
        adapter_for(Harness::Copilot)
            .unwrap()
            .marion_tool_name(ROOT_VERBS[0]),
        "marion-spawn"
    );
}

/// The gate itself is `duplex::wait_for_ready`, tested there. What is asserted here is the
/// **root's own sentence** for it: the symptom is a run that "succeeds" with plain text, so the
/// message a `marion run` operator reads has to name the tool that was missing.
#[test]
fn a_ready_marker_that_never_appears_is_a_refusal_not_a_silent_first_turn() {
    let missing = std::env::temp_dir().join("marion-never");
    let e = root_error(
        crate::duplex::DuplexError::McpNeverReady(StdDuration::from_millis(30), missing),
        StdDuration::from_millis(30),
    );
    assert!(matches!(e, RootError::McpNeverReady(_, _)), "{e}");
    assert!(e.to_string().contains("without mcp__marion__spawn"));
}

/// A pty-only surface has no root launch path, and §5.2 forbids inventing one by handing a
/// headless node a pty on stdin. `TerminalInput` is unreachable from today's four adapters, so
/// this is asserted at the derivation rather than through one.
#[test]
fn a_terminal_input_surface_is_refused_rather_than_pushed_down_one_of_the_two_paths() {
    let e = RootError::UnsupportedRootSurface(Harness::Codex);
    assert!(e.to_string().contains("pty on stdin"));
}

fn root_spec(dir: &Path, agent_type: &str) -> RootSpec {
    RootSpec {
        os_sandbox: true,
        wider_children: false,
        budget: None,
        agent_type: agent_type.into(),
        prompt: "delegate it".into(),
        native_launch: None,
        repo: dir.join("repo"),
        state: dir.join("state"),
        base_url: Some("http://127.0.0.1:8099/v1".into()),
        bridge: "/bin/marion-supervisor".into(),
        model: builtin(agent_type).unwrap().model.clone(),
        auth: Auth::Canned,
        no_change_record: false,
        pane: false,
        resume: None,
        // What `blocked_bound_secs(None, <type>)` resolves to for every type this fixture
        // drives — §3.1's default, stated rather than left to a `Default`.
        bound_secs: marion_core::agent_type::DEFAULT_TIMEOUT_SECS,
        profile: None,
    }
}

/// A scratch dir whose `repo/` is a **real one-commit repository**, so a root prepared in it
/// has a change record and can therefore be granted what its agent type declares.
///
/// The fixture decision, stated: the tests that drive an `-impl` root `git init`, and the ones
/// that drive an orchestrator root keep the bare directory [`temp`] gives them. Passing
/// `--no-change-record` everywhere instead would have been one line, and would have left the
/// whole root suite exercising the **ungranted** path while claiming to test the grant — this
/// repository's documented failure mode, a check that passes by failing to look. Keeping the
/// bare directories where no tool is declared is not laziness either: it is now the evidence
/// that the gate is co-extensive with the grant and does not fire where nothing is at stake.
fn temp_repo(name: &str) -> (marion_testsupport::Scratch, PathBuf) {
    let dir = marion_testsupport::scratch(&format!("root-{name}"));
    let repo = marion_testsupport::fixture_repo(&dir);
    (dir, repo)
}

/// **A root gets the grant its agent type declares — and only over a recorded repository.**
///
/// This replaces the invariant that a root's availability axis is empty by construction. The
/// operator overruled its containment half; [`availability_axis`] carries the whole argument
/// and what took its place. What is asserted here is the *positive* half of that trade, which
/// nothing asserted before: `marion run claude-impl` in a real repository compiles
/// `--tools Read,Write` **and** `--allowedTools …,Read,Write`, in claude's own spelling.
///
/// **Both flags, or the grant is item 22's dead end rather than a tool.** §11 item 24 measured
/// that: availability alone routes the call to `--permission-prompt-tool stdio`, where marion
/// has no answerer, and the node receives a denial string instead of the file. `Read` and
/// `Write` are the harness's spellings and not marion's — `tests/fixtures/s14/README.md`
/// measured `--tools read`, marion's own word, producing `body.tools []` with exit 0 and an
/// empty stderr.
///
/// The orchestrator type is driven through the same path and must still compile `--tools ""`
/// with the flag present: the empty string is documented as *"disable all tools"*, so a dropped
/// flag would be a silently different grant that a substring search for `Write` would pass.
#[test]
fn a_root_over_a_recorded_repository_compiles_the_grant_its_agent_type_declares() {
    assert_eq!(
        builtin("claude-impl").unwrap().tools,
        vec!["read", "write", "edit", "bash"],
        "this test is vacuous unless the type really does declare a grant"
    );
    let (dir, repo) = temp_repo("root-tools");
    let axis = |agent_type: &str| -> (String, String) {
        let node = prepare(&RootSpec {
            repo: repo.clone(),
            state: dir.join("state"),
            ..root_spec(&dir, agent_type)
        })
        .expect("the root compiles");
        let args = node.invocation.args.clone();
        let after = |flag: &str| {
            let i = args
                .iter()
                .position(|a| a == flag)
                .unwrap_or_else(|| panic!("{agent_type}: {flag} is always compiled"));
            args[i + 1].clone()
        };
        (after("--tools"), after("--allowedTools"))
    };

    let (tools, allowed) = axis("claude-impl");
    assert_eq!(
        tools, "Read,Write,Edit,Bash",
        "availability, in claude's own spelling"
    );
    assert_eq!(
        allowed,
        "mcp__marion__spawn,mcp__marion__status,mcp__marion__wait,mcp__marion__list,mcp__marion__steer,mcp__marion__cancel,Read,Write,Edit,Bash",
        "permission must carry the same grant beside marion's own verbs, or the tool exists \
         and every call to it is refused (§11 items 22 and 24)"
    );
    assert!(
        !node_args_mention_a_denylist(&repo, &dir),
        "marion must never compile --disallowedTools (§3.1)"
    );

    let (tools, allowed) = axis("claude-orchestrator");
    assert_eq!(
        tools, "",
        "an orchestrator type declares nothing and must still get the flag, empty"
    );
    assert!(
        allowed.starts_with("mcp__marion__spawn") && !allowed.contains("Write"),
        "marion's own verbs are the root's permission axis and no agent type widens them: \
         {allowed}"
    );
}

/// **A root's `SpawnIntent` records the bound the run is actually held to.**
///
/// `prepare` is the only writer of a root's intent, and until it recorded this the journal said
/// nothing at all about the node's clock: every reader — `marion tree`'s detail pane first
/// among them — re-resolved §3.1's bound from the *agent type* and printed 900 s for a root the
/// operator had launched with `--timeout 300`. The value is [`RootSpec::bound_secs`] rather
/// than a second resolution here, so the number the launch enforces and the number the journal
/// reports are one value.
#[test]
fn a_roots_intent_records_the_bound_its_launch_was_resolved_to() {
    let (dir, repo) = temp_repo("root-bound");
    let node = prepare(&RootSpec {
        repo: repo.clone(),
        state: dir.join("state"),
        bound_secs: 300,
        ..root_spec(&dir, "claude")
    })
    .expect("the root compiles");

    let journalled = std::fs::read_to_string(node.project.journal()).expect("a journal");
    let intents: Vec<u64> = journalled
        .lines()
        .filter_map(|l| marion_core::journal::decode(l.as_bytes()))
        .filter_map(|r| match r.kind {
            marion_core::journal::RecordKind::SpawnIntent(i) if i.agent_id == node.agent_id => {
                Some(i.timeout_secs.expect("the bound is on the record"))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        intents,
        vec![300],
        "one intent, carrying the operator's `--timeout 300` — not §3.1's default:\n\
         {journalled}"
    );
    assert_ne!(
        300,
        marion_core::agent_type::DEFAULT_TIMEOUT_SECS,
        "the assertion above is not passing by coincidence with the default"
    );
}

/// `--disallowedTools` must never appear on a root's command line, whatever it was granted.
///
/// §3.1 is absolute about it: enumerating the complement of the built-in set silently escalates
/// privilege the first time the harness adds a tool. Asserted over a **granted** root, since
/// that is the only configuration in which anyone would be tempted to write one.
fn node_args_mention_a_denylist(repo: &Path, dir: &Path) -> bool {
    let node = prepare(&RootSpec {
        repo: repo.to_path_buf(),
        state: dir.join("state"),
        ..root_spec(dir, "claude-impl")
    })
    .expect("the root compiles");
    node.invocation
        .args
        .iter()
        .any(|a| a.contains("disallowed") || a.contains("Disallowed"))
}

/// **NC-5 — the grant gate, in both directions, over roots `prepare` really built.**
///
/// The rule is *no audit, no grant*, and it is asserted as an implication rather than as a spot
/// check: whenever a root's `--tools` is non-empty, its `RootNode` carries a base point. The
/// only source of that axis is [`availability_axis`], and exactly one of its arms can return a
/// non-empty list, so this cannot be defeated by an edit that forgets a check — only by one
/// that deletes an arm.
///
/// Four roots, because the interesting cases are the corners:
///
/// 1. `claude-impl` in a bare directory — **refused by name**, and the message names the
///    directory and the declaration. This is the case the whole gate exists for.
/// 2. `claude` in the same bare directory — **runs**, with an empty axis. The gate is
///    co-extensive with the grant, so a root that asked for nothing meets no gate; the
///    behaviour every root fixture in this repository has always had is unchanged.
/// 3. `claude-impl` in a git fixture — `Taken`, and granted.
/// 4. `claude-impl` with `--no-change-record` in the same git fixture — `NotAttempted`, and
///    **not** granted, which is what makes the flag an escape hatch rather than a way past the
///    gate.
#[test]
fn a_non_empty_availability_axis_is_reachable_only_from_a_recorded_base() {
    // The unrecorded arm, direct — a declaration marion cannot record is a refusal.
    let err = availability_axis(
        &["write".to_string()],
        &RootChangeBase::Unavailable {
            reason: "not a git worktree".into(),
        },
        Path::new("/nowhere"),
    )
    .expect_err("no audit, no grant");
    assert!(matches!(err, RootError::NoChangeRecord { .. }), "{err}");
    // ...and the same arm with nothing declared is not a refusal at all.
    assert!(
        availability_axis(
            &[],
            &RootChangeBase::Unavailable {
                reason: "not a git worktree".into()
            },
            Path::new("/nowhere"),
        )
        .expect("a root that asked for nothing meets no gate")
        .is_empty()
    );

    // 1. Declared, unrecordable: refused, naming both halves.
    let outside = temp("gate-outside");
    let e = prepare(&root_spec(&outside, "claude-impl"))
        .expect_err("a grant with no record behind it must not be issued");
    let msg = e.to_string();
    assert!(matches!(e, RootError::NoChangeRecord { .. }), "{msg}");
    for needle in [
        &*outside.join("repo").display().to_string(),
        "read, write",
        "--no-change-record",
    ] {
        assert!(
            msg.contains(needle),
            "the refusal must name the directory, the declaration and the remedy; missing \
             `{needle}` in: {msg}"
        );
    }

    // 2. Nothing declared, same directory: unchanged, and that is the co-extensive half.
    let a = prepare(&root_spec(&outside, "claude-orchestrator"))
        .expect("a root that declares no tool still runs outside a repository");
    match &a.change_base {
        RootChangeBase::Unavailable { reason } => assert!(
            reason.contains("git"),
            "the absence must name its cause: {reason}"
        ),
        other => panic!("a bare directory is not a worktree: {other:?}"),
    }

    // 3. Declared and recorded: granted.
    let (inside, repo) = temp_repo("gate-inside");
    let b = prepare(&RootSpec {
        repo: repo.clone(),
        state: inside.join("state"),
        ..root_spec(&inside, "claude-impl")
    })
    .expect("a git worktree is snapshottable");
    assert!(
        matches!(b.change_base, RootChangeBase::Taken { .. }),
        "a real repository must yield a base point, or every record below is vacuous"
    );

    // 4. Declared, recordable, and declined: not granted, and not refused either.
    let c = prepare(&RootSpec {
        repo,
        state: inside.join("state"),
        no_change_record: true,
        ..root_spec(&inside, "claude-impl")
    })
    .expect("declining the record is not an error, it is a decision");
    assert!(
        matches!(c.change_base, RootChangeBase::NotAttempted { .. }),
        "the flag must record a decision not to look, never a git failure that never happened"
    );

    // The implication itself, over every root this test built.
    for (label, node) in [("orchestrator", &a), ("granted", &b), ("declined", &c)] {
        let args = &node.invocation.args;
        let i = args.iter().position(|x| x == "--tools").expect("compiled");
        let granted = !args[i + 1].is_empty();
        assert_eq!(
            granted,
            matches!(node.change_base, RootChangeBase::Taken { .. }),
            "{label}: a root was granted `{}` with no change record behind it — that is \
             `8a69f22`'s ambiguity taken on purpose (see `availability_axis`)",
            args[i + 1]
        );
    }
}

/// **The grant is durable before the process is, and the base point with it.**
///
/// `prepare` decides the grant and takes the pre-tree; `journal_the_roots_outcome` writes the
/// change record only once `launch_watched` **returns**. Between the two there is a whole run,
/// and a marion that panics, is SIGKILLed, or loses power inside it used to leave the journal
/// saying nothing at all — not that a grant was issued, not what the tree looked like when it
/// was. The evidence is recoverable, too: the pre-tree object is in the agent dir's own object
/// store, so an oid in the journal is a handle on the real tree and not a bare number.
///
/// This test is the crash: `prepare` returns and nothing is ever launched. What must already
/// be on disk at that instant is the grant and the base.
#[test]
fn a_prepared_root_journals_its_grant_and_its_base_before_any_process_exists() {
    let (dir, repo) = temp_repo("grant-before-launch");
    let node = prepare(&RootSpec {
        repo,
        state: dir.join("state"),
        ..root_spec(&dir, "claude-impl")
    })
    .expect("a git worktree is snapshottable, so the grant is issued");
    let RootChangeBase::Taken { pre_tree, .. } = &node.change_base else {
        panic!(
            "this test is vacuous unless a base was taken: {:?}",
            node.change_base
        );
    };
    let journal = String::from_utf8_lossy(
        &std::fs::read(node.project.journal()).expect("prepare journals before it returns"),
    )
    .into_owned();
    assert!(
        journal.contains(&pre_tree.0),
        "the tree the grant was issued against is not in the journal, so a crash before \
         `launch_watched` returns leaves no evidence of any kind — not even that a grant was \
         issued. §6.1's intent-then-confirm split says the intent is written before the act; \
         a grant is an act.\n{journal}"
    );
    assert!(
        journal.contains("read") && journal.contains("write"),
        "…and what was granted must be in it too: `read` and a `write` on the operator's own \
         checkout are different runs to audit.\n{journal}"
    );

    // The same claim structurally, through the reader an operator would actually use. A
    // substring search passes on a journal that merely mentions the oid somewhere; this
    // asserts replay folds it into the node, and that the pair of fields says *which* run this
    // was — granted, and no outcome recorded.
    let tree = marion_core::registry::replay(journal.as_bytes());
    let n = tree
        .get(&node.agent_id)
        .expect("the intent introduced the node");
    let grant = n.root_grant.as_ref().expect("the grant is journalled");
    assert_eq!(grant.pre_tree.as_ref(), Some(pre_tree));
    assert_eq!(grant.granted.as_str(), "read, write, edit, bash");
    assert!(
        n.granted_without_a_record(),
        "this is the crash: a grant on the operator's checkout with nothing yet saying what \
         came of it. A reader that could not see this would read the run as one that never \
         started."
    );
    assert!(
        !n.did_marion_look(),
        "and no measurement exists yet — the post-snapshot happens at exit, which never came"
    );
}

/// **A root past `Spawned` ends in `Exited`, never `SpawnAborted`.** Measured live
/// (2026-09-22, c09): an opencode root refused after its run was journalled `Spawned`,
/// `Running`, then `SpawnAborted` — a record for a node marion decided the fate of before it
/// produced anything, about a node that ran and exited. Replay then held it non-terminal: the
/// supervisor stayed resident on it, and `marion run` disclosed it as "unattended at a
/// permission gate". A refusal of a run that happened is that run's exit, `Failed`, with
/// marion's sentence as its description; a root whose process never started still aborts.
#[test]
fn a_refused_root_that_ran_is_journalled_exited_and_one_that_never_ran_aborted() {
    let refusal = || RootError::NoVerbAnswered {
        harness: Harness::OpenCode,
        calls: 1,
        detail: "spawn was refused: no".into(),
        exit: Some(0),
        failure: String::new(),
        stderr: String::new(),
    };
    let dir = temp("refused-after-spawned");
    let ran = prepare(&root_spec(&dir, "claude-orchestrator")).unwrap();
    journal_the_roots_outcome(&ran, &Err(refusal()), true, "1.0.0", false);
    let tree = marion_core::registry::replay(&std::fs::read(ran.project.journal()).unwrap());
    let n = tree.get(&ran.agent_id).unwrap();
    assert_eq!(
        n.state,
        marion_core::node::NodeState::Exited(ExitStatus::Failed)
    );
    assert_eq!(n.spawn_aborted, None, "{n:?}");
    assert!(matches!(
        roots_terminal_lifecycle(&Err(refusal()), true, false),
        marion_core::event::Lifecycle::Exited {
            status: ExitStatus::Failed,
            ..
        }
    ));

    let never_dir = temp("refused-before-spawned");
    let never = prepare(&root_spec(&never_dir, "claude-orchestrator")).unwrap();
    let unstarted = RootError::UnaccountableNode { why: "full".into() };
    journal_the_roots_outcome(&never, &Err(unstarted), false, "1.0.0", false);
    let tree = marion_core::registry::replay(&std::fs::read(never.project.journal()).unwrap());
    assert!(tree.get(&never.agent_id).unwrap().spawn_aborted.is_some());
}

/// **A root whose `SpawnIntent` cannot be made durable is refused.** The intent is the one
/// record that names the node before a process exists; a root launched without it is the
/// untracked live process §9's M2 criteria forbid. The fault is real, not injected: the journal
/// path is a directory, so every append fails.
///
/// Mutation: write the intent through `journal::record` and `prepare` succeeds.
#[test]
fn a_root_whose_intent_cannot_be_journalled_is_refused() {
    let dir = temp("intent-barrier");
    let spec = root_spec(&dir, "claude-orchestrator");
    let project = ProjectDir::new(&spec.state, &crate::socket::project_root(&spec.repo));
    std::fs::create_dir_all(project.journal()).unwrap();
    match prepare(&spec) {
        Err(RootError::Run(SpawnError::SpawnIntentBarrier { .. })) => {}
        Err(other) => panic!("the wrong refusal: {other}"),
        Ok(node) => panic!(
            "a root with no durable intent prepared: {}",
            node.agent_id.0
        ),
    }
}

/// A root's scope is its type's **ceiling and nothing else**, because no parent authored a
/// request. `["**"]` in a request slot would fabricate an author; `CeilingOnly` says there was
/// never one to fabricate.
#[test]
fn a_roots_scope_is_a_ceiling_with_no_request_beside_it() {
    let dir = temp("scope");
    let node = prepare(&root_spec(&dir, "claude-orchestrator")).unwrap();
    match &node.scope {
        RootScope::CeilingOnly { ceiling } => assert_eq!(
            ceiling,
            &builtin("claude-orchestrator").unwrap().scope_ceiling,
            "the agent type's own ceiling, not a copy that could drift"
        ),
        other => panic!("{other:?}"),
    }
}

/// **`prepare` accepts a fileless declaration, and only because the adapter named the route.**
///
/// A live opencode root writes no configuration document at all — its declaration rides
/// `OPENCODE_CONFIG_CONTENT`, because a file under an isolated `$XDG_CONFIG_HOME` would isolate
/// away the login the run exists to use. The old check read an empty `config_files` as
/// `NoMcpDeclaration` and refused it; the new one checks the route the adapter *stated*, so this
/// passes while a genuinely bridgeless node still cannot.
#[test]
fn a_live_opencode_root_declares_its_bridge_without_writing_a_file() {
    let dir = temp("opencode-live");
    let node = prepare(&RootSpec {
        auth: Auth::Inherited,
        base_url: None,
        // The built-in default names marion's own generated provider block, which a live node
        // does not write — the adapter refuses it by name, so a real one is given here.
        model: Some("anthropic/claude-sonnet-4-5".into()),
        ..root_spec(&dir, "opencode")
    })
    .expect("a fileless MCP route is legal when the adapter says that is its route");
    assert_eq!(
        node.mcp_config, None,
        "no document: the declaration is in the environment"
    );
    let (_, content) = node
        .invocation
        .env
        .iter()
        .find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
        .expect("and marion checked that it is actually there");
    let v: Value = serde_json::from_str(content).unwrap();
    assert_eq!(
        v["mcp"]["marion"]["environment"]["MARION_AUTH"],
        "inherited"
    );
    // Nothing marion could write escaped the agent dir, because marion wrote nothing.
    assert!(
        !node
            .invocation
            .env
            .iter()
            .any(|(k, _)| k == "HOME" || k.starts_with("XDG_"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The other side of the same coin: the refusal still exists and still names the route. An
/// adapter that declared *nothing* must not be waved through just because filelessness is now
/// legal for somebody.
#[test]
fn a_node_with_no_declaration_on_any_route_is_still_refused_by_name() {
    for (route, needle) in [
        ("a configuration document", "configuration document"),
        ("$OPENCODE_CONFIG_CONTENT", "OPENCODE_CONFIG_CONTENT"),
        ("no route at all", "no route at all"),
    ] {
        let e = RootError::NoMcpDeclaration {
            harness: Harness::OpenCode,
            route: route.into(),
        };
        let s = e.to_string();
        assert!(s.contains(needle), "{s}");
        assert!(
            s.contains("no bridge at all"),
            "the refusal must name the failure class, not merely fail: {s}"
        );
    }
}

/// A scratch dir that removes itself, plus the `repo/` every root fixture here expects.
///
/// The guard is `marion-testsupport`'s, not a tenth copy: this file's own `temp` returned a
/// bare `PathBuf` and cleaned up nowhere, so every run stranded a directory — 26 of them by the
/// time it was caught, from a single test added late. That is the shape cb6ab2e removed from
/// every other test module, and the reason the guard lives in a dev-dependency crate is
/// precisely so `#[cfg(test)]` code in the lib can reach it.
///
/// Bind the result for the whole test. `temp("x").join("y")` drops the guard at the end of that
/// statement and deletes the directory out from under the test.
fn temp(name: &str) -> marion_testsupport::Scratch {
    let dir = marion_testsupport::scratch(&format!("root-{name}"));
    std::fs::create_dir_all(dir.join("repo")).unwrap();
    dir
}

/// **Every built-in prepares** — argv compiled, configuration on disk, prompt where its surface
/// puts it. This is the whole of `marion run <agent-type>` short of the process itself.
///
/// The opencode case is a regression that was real: its document lands at
/// `<config_dir>/config/opencode/opencode.json`, a layout `$XDG_CONFIG_HOME` gives the harness
/// and marion does not create, so writing it failed the launch with a bare
/// `No such file or directory` naming neither the path nor the harness.
///
/// Driven off `builtin_names()` rather than a list kept here, because a list kept here is a
/// list a new built-in is silently absent from — which is exactly what happened when
/// `acp-opencode` landed, then as the one type refused as a root. It is a root now, on its own
/// path: the prompt is a `session/prompt` frame, there is no marker to gate on (the answer to
/// `session/new` is the gate), and the declaration rides the `session/new` request, stamped
/// depth 0 and with the root's own id, exactly as a child's is stamped with its own.
#[test]
fn every_builtin_agent_type_prepares_as_a_root_with_its_config_actually_on_disk() {
    let dir = temp("prepare");
    // A **git** repository, not the bare directory `temp` makes. Driving the sweep off
    // `builtin_names()` surfaced `claude-impl` and `gemini-impl`, which declare tools — and
    // §6.1's rule is "no audit, no grant", so `prepare` refuses a tool-declaring root outright
    // where no change record can be taken. That refusal is correct; the fixture was wrong, and
    // a bare directory silently tested only the types that ask for nothing.
    marion_testsupport::fixture_repo(&dir);
    let mut acp_roots = 0;
    for name in marion_core::agent_type::builtin_names() {
        // Canned wherever the type can be: every harness row but agy, and an ACP type whose
        // agent has a measured canned recipe. agy and an ACP agent without one run only on the
        // operator's own login and refuse canned by name, so that type is prepared live —
        // compiled, never launched. Asked of the row and the binding, not of a name list.
        let mut spec = root_spec(&dir, name);
        let agent_type = builtin(name).unwrap();
        if agent_type.harness == Harness::Antigravity {
            let canned = prepare(&spec).err().map(|e| e.to_string());
            assert!(
                canned
                    .as_deref()
                    .is_some_and(|e| e.contains("no canned or endpoint provider route")),
                "{name}: {canned:?}"
            );
            spec.auth = Auth::Inherited;
            spec.base_url = None;
        }
        if let Some(selector) = &agent_type.acp_agent
            && marion_harness::acp::Binding::resolve(selector)
                .unwrap()
                .canned()
                .is_none()
        {
            spec.auth = Auth::Inherited;
            spec.base_url = None;
        }
        let node = prepare(&spec).unwrap_or_else(|e| panic!("{name} cannot be a root: {e}"));
        // Canned: every harness declares marion's bridge here — in a document, or, on goose,
        // as the one `--with-extension marion:…` argv token its row routes through
        // (`McpRoute::Argv`), which `verify` has already checked the argv for. An ACP root's
        // rides `session/new`, asserted in its own arm below.
        match node.mcp_config.as_ref() {
            None if node.path == RootPath::Acp => {}
            Some(declaration) => assert!(
                declaration.is_file(),
                "{name}: the MCP declaration marion compiled a path to must exist: {}",
                declaration.display()
            ),
            None => assert!(
                node.invocation
                    .args
                    .iter()
                    .any(|a| a.starts_with("marion:")),
                "{name}: no MCP declaration document and none on argv: {:?}",
                node.invocation.args
            ),
        }
        // Verbatim, or (agy) headed by the working-directory preamble its adapter states.
        let in_argv = node
            .invocation
            .args
            .iter()
            .any(|a| a == "delegate it" || a.ends_with("\n\ndelegate it"));
        match node.path {
            RootPath::LaunchOnly => {
                assert!(
                    in_argv,
                    "{name}: a LaunchOnly prompt rides argv (§6.1 step 8)"
                );
                assert!(node.ready_file.is_none(), "{name}: nothing to gate");
            }
            RootPath::Duplex => {
                assert!(
                    !in_argv,
                    "{name}: a duplex prompt is a frame, not an argument"
                );
                assert!(node.ready_file.is_some(), "{name}: the §6.1 step 8 marker");
            }
            // Unreachable from a built-in agent type: `prepare_watched` derives the path from
            // `adapter.surfaces()`, and no adapter's *default* shape is `TerminalInput` — the
            // pane shape is `HarnessAdapter::pane_surfaces`, asked for per run. Named rather
            // than wildcarded so a fifth harness that declares a pane by default fails here.
            RootPath::Terminal => panic!(
                "{name}: a built-in agent type reached the pane path without a run asking \
                 for one"
            ),
            RootPath::AppServer => {
                assert!(!in_argv, "{name}: an app-server prompt is a turn/start");
                assert!(
                    node.ready_file.is_none(),
                    "{name}: the server's startup notification is the gate, not a marker"
                );
                let opening = node
                    .session_declaration
                    .as_ref()
                    .expect("an app-server root opens a thread");
                assert!(
                    node.surfaces.control
                        == marion_harness::ControlTransport::Typed(
                            marion_harness::TypedKind::AppServer
                        ),
                    "{name}"
                );
                assert!(opening.get("method").is_some(), "{name}: {opening}");
            }
            RootPath::Acp => {
                assert!(
                    !in_argv,
                    "{name}: an ACP prompt is a `session/prompt` frame"
                );
                assert!(
                    node.ready_file.is_none(),
                    "{name}: the answer to `session/new` is the gate, not a marker"
                );
                // The bridge rides the channel the agent's row names: the protocol's own
                // `session/new`, or — on a row measured to ignore it — the agent's argv flag,
                // whose document then carries the same env (and the session block is empty).
                let selector = agent_type
                    .acp_agent
                    .as_deref()
                    .expect("an ACP type names its agent");
                let binding = marion_harness::acp::Binding::resolve(selector).unwrap();
                type EnvLookup = Box<dyn Fn(&str) -> Option<String>>;
                let (var, decl): (EnvLookup, String) =
                    match (&node.session_declaration, binding.declaration()) {
                        (Some(decl), marion_harness::acp::Declaration::Session) => {
                            assert_eq!(decl["method"], "session/new", "{name}");
                            let env = decl["params"]["mcpServers"][0]["env"]
                                .as_array()
                                .unwrap_or_else(|| panic!("{name}: no bridge env in {decl}"))
                                .clone();
                            (
                                Box::new(move |k: &str| {
                                    env.iter()
                                        .find(|e| e["name"] == k)
                                        .and_then(|e| e["value"].as_str())
                                        .map(str::to_string)
                                }),
                                decl.to_string(),
                            )
                        }
                        (None, marion_harness::acp::Declaration::Argv { flag, .. }) => {
                            let args = &node.invocation.args;
                            let at = args
                                .iter()
                                .position(|a| a == flag)
                                .unwrap_or_else(|| panic!("{name}: no `{flag}` on argv: {args:?}"));
                            let doc: Value = serde_json::from_str(&args[at + 1]).unwrap();
                            let env = doc
                                .pointer(&format!(
                                    "/mcpServers/{}/env",
                                    marion_harness::acp::MCP_SERVER_NAME
                                ))
                                .unwrap_or_else(|| panic!("{name}: no bridge env in {doc}"))
                                .clone();
                            (
                                Box::new(move |k: &str| env[k].as_str().map(str::to_string)),
                                doc.to_string(),
                            )
                        }
                        (decl, channel) => panic!(
                            "{name}: the bridge is declared on {channel:?}, and the session \
                             block is {decl:?}"
                        ),
                    };
                assert_eq!(
                    var(DEPTH_ENV).as_deref(),
                    Some("0"),
                    "{name}: a root's bridge is depth 0: {decl}"
                );
                assert_eq!(
                    var(AGENT_ID_ENV).as_deref(),
                    Some(node.agent_id.0.as_str()),
                    "{name}: the bridge carries the root's own id: {decl}"
                );
                acp_roots += 1;
            }
        }
        assert_eq!(node.prompt, "delegate it", "{name}");
    }
    assert!(
        acp_roots > 0,
        "no built-in exercises the ACP root path, so this test asserts nothing about it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A run that did not ask for a pane is prepared from the bytes it was prepared from before
/// panes existed. That is M1's whole guarantee against this feature.**
///
/// The pty module already asserts the *declaration* half — no adapter's `surfaces()` claims a
/// display plane (`pty::tests::a_node_that_did_not_ask_for_a_pane_is_not_given_one`) — and that
/// is not the half that can now break. `prepare_watched` reads `RootSpec::pane` and picks
/// between two surfaces methods and two compile methods, and a mistake there gives a node a pty
/// nobody asked for while every adapter test stays green. M1's measured path is
/// `--print --output-format stream-json` over pipes; a pane is a TUI with the prompt in argv,
/// and the two are not near-misses of each other.
///
/// Asserted on both sides, so it cannot pass by answering "no pane" unconditionally.
///
/// **Mutation:** in `prepare_watched`, ignore `spec.pane` — take `pane_surfaces()` whenever it
/// is `Some`, or take `surfaces()` always. Either fails here.
#[test]
fn only_a_run_that_asked_for_a_pane_is_compiled_as_one() {
    use marion_harness::ControlTransport;

    let dir = temp("pane-selection");
    let headless = prepare(&root_spec(&dir, "claude-orchestrator")).expect("a claude root");
    let paned = prepare(&RootSpec {
        pane: true,
        ..root_spec(&dir, "claude-orchestrator")
    })
    .expect("a claude root with a pane");

    // The path, the surfaces and the stdin the surfaces imply — three derivations of the one
    // decision, and all three have to move together or `launch_terminal` gets a witness for a
    // node whose stdin is a pipe.
    assert_eq!(headless.path, RootPath::Duplex);
    assert!(
        headless.surfaces.display_plane().is_none(),
        "a run that asked for nothing was given a display plane, so `launch_terminal` is \
         reachable from M1's own path"
    );
    assert_eq!(
        crate::pty::stdin_plan(headless.surfaces.control),
        crate::pty::StdinPlan::Piped,
        "S11 measured `claude -p` exiting 1 with \"Input must be provided…\" on an isatty(0) \
         stdin"
    );

    assert_eq!(paned.path, RootPath::Terminal);
    assert!(
        paned.surfaces.display_plane().is_some(),
        "the pane shape must mint the witness `spawn_pty` requires, or C1 cannot launch"
    );
    assert_eq!(paned.surfaces.control, ControlTransport::TerminalInput);
    assert_eq!(
        crate::pty::stdin_plan(paned.surfaces.control),
        crate::pty::StdinPlan::TerminalSlave
    );

    // And the argv, which is what actually reaches the harness. The headless launch keeps
    // M1's two switches and does **not** carry the prompt (§6.1 step 8 writes it as a frame);
    // the pane carries neither switch and seeds the composer instead.
    let args = |n: &RootNode| n.invocation.args.join("\u{1}");
    assert!(
        headless.invocation.args.iter().any(|a| a == "-p"),
        "M1's measured launch lost `-p`: {:?}",
        headless.invocation.args
    );
    assert!(
        args(&headless).contains("stream-json"),
        "M1's measured launch lost its output format: {:?}",
        headless.invocation.args
    );
    assert!(
        !headless.invocation.args.iter().any(|a| a == "delegate it"),
        "a duplex prompt is a frame, not an argument: {:?}",
        headless.invocation.args
    );

    assert!(
        !paned.invocation.args.iter().any(|a| a == "-p"),
        "the pane compiled the headless launch: {:?}",
        paned.invocation.args
    );
    assert!(
        !args(&paned).contains("stream-json"),
        "the pane compiled a frame protocol onto a node marion drives by keystrokes: {:?}",
        paned.invocation.args
    );
    assert!(
        paned.invocation.args.iter().any(|a| a == "delegate it"),
        "the pane's prompt must be seeded into the composer: {:?}",
        paned.invocation.args
    );

    // **The same two-sided check on codex, because codex now has a pane too.** Its headless
    // shape is `codex app-server` (S36), and a selection bug that gave every codex node the TUI
    // would take that path away without any adapter test noticing.
    let cx = prepare(&root_spec(&dir, "codex")).expect("a codex root");
    let cx_paned = prepare(&RootSpec {
        pane: true,
        ..root_spec(&dir, "codex")
    })
    .expect("a codex root with a pane");
    assert_eq!(cx.path, RootPath::AppServer);
    assert_eq!(
        cx.invocation.args.first().map(String::as_str),
        Some("app-server")
    );
    assert!(
        !cx.invocation.args.iter().any(|a| a == "delegate it"),
        "the headless prompt is a turn, never argv: {:?}",
        cx.invocation.args
    );
    assert!(
        cx.surfaces.display_plane().is_none(),
        "a codex run that asked for nothing was given a pty"
    );
    assert_eq!(cx_paned.path, RootPath::Terminal);
    assert_ne!(
        cx_paned.invocation.args.first().map(String::as_str),
        Some("app-server"),
        "the codex pane compiled the headless shape instead of the TUI: {:?}",
        cx_paned.invocation.args
    );
    assert!(
        cx_paned.invocation.args.iter().any(|a| a == "delegate it"),
        "the codex pane dropped its prompt: {:?}",
        cx_paned.invocation.args
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// **A harness with no interactive shape refuses by name, and is never quietly launched
/// headless.**
///
/// A caller that asked for a pane is a caller that is about to run `marion attach`. Downgrading
/// it would answer that attach with *"node has no display plane"* — a true sentence about a
/// node marion made headless after being told not to, and the operator has no way to tell it
/// from a harness that never supported one.
///
/// Asserted over every built-in that declares no pane, and with a positive control, so it
/// cannot pass by refusing everything.
///
/// **Mutation:** in `prepare_watched`, fall back to `adapter.surfaces()` when `pane_surfaces()`
/// is `None`. This fails.
#[test]
fn asking_for_a_pane_a_harness_does_not_have_is_refused_rather_than_downgraded() {
    let dir = temp("pane-refusal");
    let mut refused = 0;
    for name in [
        "claude-orchestrator",
        "codex",
        "codex-impl",
        "gemini-orchestrator",
        "opencode",
    ] {
        let has_pane = adapter_for(builtin(name).unwrap().harness)
            .unwrap()
            .pane_surfaces()
            .is_some();
        let got = prepare(&RootSpec {
            pane: true,
            ..root_spec(&dir, name)
        });
        match (has_pane, got) {
            (true, Ok(node)) => assert_eq!(node.path, RootPath::Terminal, "{name}"),
            (true, Err(e)) => panic!("{name} declares a pane shape and would not prepare: {e}"),
            (false, Ok(node)) => panic!(
                "{name} has no pane shape and was silently prepared as a {:?} node anyway. A \
                 `marion attach` against it would then be refused for having no display \
                 plane, which reads as marion never having supported one",
                node.path
            ),
            (false, Err(e)) => {
                refused += 1;
                assert!(
                    e.to_string().contains("no pane shape"),
                    "{name}: the refusal must name the cause, not just fail: {e}"
                );
            }
        }
    }
    assert!(refused > 0, "nothing was refused, so this asserted nothing");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **The root is depth 0, on every harness, in the document its own bridge will read.**
///
/// The adapters are tested for carrying whatever `SpawnCtx` hands them; this is the other end
/// of that wire — that `marion run` hands them the right thing. It is the one place a literal
/// could be wrong while every adapter test still passed, and getting it wrong is not a cosmetic
/// error: a root declared at any other depth would mis-measure its whole subtree, and one
/// declared at `max_depth` could not delegate at all.
///
/// Read off the bytes on disk rather than off `ctx`, because the bytes are what the bridge gets.
#[test]
fn every_root_declares_itself_at_depth_zero_and_names_its_own_type() {
    let dir = temp("depth");
    for name in [
        "claude-orchestrator",
        "codex",
        "codex-impl",
        "gemini-orchestrator",
        "opencode",
    ] {
        let node = prepare(&root_spec(&dir, name)).unwrap();
        let doc = std::fs::read_to_string(node.mcp_config.as_ref().unwrap()).unwrap();
        assert!(
            doc.contains("\"0\""),
            "{name}: a root is depth 0 (§3.1), and the value must be in the document its own \
             bridge reads:\n{doc}"
        );
        assert!(
            doc.contains(DEPTH_ENV),
            "{name}: {DEPTH_ENV} is missing:\n{doc}"
        );
        // The canonical name, so `marion run codex` and `marion run codex-impl` hand the bridge
        // one spelling and it re-resolves one definition.
        assert!(
            doc.contains(&format!("\"{}\"", builtin(name).unwrap().name)),
            "{name}: its canonical agent type name must reach the bridge, or §6.1 step 2's \
             gates have no max_depth to read:\n{doc}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// **§5.4's per-node capability reaches the root's own bridge — and only when somebody owns
/// the node.**
///
/// The token is minted by whoever owns the node's lifecycle, which since §11 item 28 step 6 is
/// the supervisor, and [`prepare_watched`] writes it into exactly one place: the declaration
/// this root's bridge reads. Asserted off the bytes the bridge actually gets, for the reason
/// `every_root_declares_itself_at_depth_zero_and_names_its_own_type` gives — a `SpawnCtx` field
/// that never reaches a file is a credential nothing can present.
///
/// **The `Unwatched` half is not symmetry for its own sake.** `node_token: None` is the
/// pre-step-6 record and it is still constructible, so a build that quietly went back to it
/// would pass every other test in this file: a root would prepare, launch, run and exit
/// normally, and the only symptom would be that its bridge could present no `SpawnCaller` and
/// the socket's `agent/spawn` refused it by name. Both rows are here so that the difference
/// between "nobody minted one" and "one was minted and dropped" is a test rather than a
/// reading.
#[test]
fn a_watched_roots_declaration_carries_the_token_its_owner_minted() {
    struct Owner(Secret);
    impl crate::run::SpawnObserver for Owner {
        fn identified(&self, _: &AgentId) -> Option<Secret> {
            Some(self.0.clone())
        }
        fn started(&self, _: &AgentId, _: i32) {}
    }
    /// Everywhere a harness can be told something: the declaration document, the compiled argv
    /// (codex's `-c mcp_servers.marion.env.…` route), and the invocation's environment. Asked
    /// as one question because *which* of the three a harness uses is `McpRoute`'s business,
    /// and a test that picked one would silently stop measuring a harness that moved.
    fn declared_anywhere(node: &RootNode, needle: &str) -> bool {
        let doc = node
            .mcp_config
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        doc.contains(needle)
            || node.invocation.args.iter().any(|a| a.contains(needle))
            || node.invocation.env.iter().any(|(_, v)| v.contains(needle))
    }

    // Distinctive enough that it cannot collide with an id, a path or a model name.
    const TOKEN: &str = "root-node-token-8f1c-4a20-b7de";
    let dir = temp("node-token");
    for name in [
        "claude-orchestrator",
        "codex",
        "codex-impl",
        "gemini-orchestrator",
        "opencode",
    ] {
        let owned = prepare_watched(&root_spec(&dir, name), &Owner(TOKEN.into()))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            declared_anywhere(&owned, TOKEN),
            "{name}: the token its owner minted must reach the bridge, or this root cannot \
             delegate at all (§5.4)"
        );
        let unowned = prepare(&root_spec(&dir, name)).unwrap();
        assert!(
            !declared_anywhere(&unowned, TOKEN),
            "{name}: `Unwatched` mints nothing, so nothing may appear — a token that showed \
             up here would have come from somewhere other than the owner"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// §9: the root's credential never leaks past the harness that reads it. Every harness takes
/// it through its row, which puts it where that harness looks — Claude Code's as
/// `ANTHROPIC_AUTH_TOKEN` — so the Anthropic pair on any other root would present the token to
/// a harness that ignores it.
#[test]
fn only_the_duplex_root_carries_the_anthropic_env_pair() {
    let dir = temp("token");
    for name in [
        "claude-orchestrator",
        "codex",
        "gemini-orchestrator",
        "opencode",
    ] {
        let node = prepare(&root_spec(&dir, name)).unwrap();
        let has = |k: &str| node.invocation.env.iter().any(|(n, _)| n == k);
        assert_eq!(
            has("ANTHROPIC_AUTH_TOKEN"),
            node.path == RootPath::Duplex,
            "{name}"
        );
        assert_eq!(
            has("ANTHROPIC_API_KEY"),
            node.path == RootPath::Duplex,
            "{name}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// **The other end of `--live`, asserted on a root marion actually prepared.**
///
/// The row withholds the three env vars it compiles, and `prepare` hands it no token to place.
/// Blanking `ANTHROPIC_API_KEY` on a live node is the specific harm: it is set to `""` under
/// `Canned` precisely so a real key cannot silently win, which is exactly the wrong thing to do
/// to a node meant to use it.
///
/// Only `claude` is swept: it is the only harness part 1 makes live, and the other three refuse
/// under `Inherited` because their generated configs require a base URL there is none of.
#[test]
fn a_live_claude_root_pushes_no_anthropic_pair_and_keeps_its_fileless_isolation() {
    let dir = temp("live");
    let node = prepare(&RootSpec {
        base_url: None,
        auth: Auth::Inherited,
        ..root_spec(&dir, "claude-orchestrator")
    })
    .unwrap();
    for k in [
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "CLAUDE_CONFIG_DIR",
    ] {
        assert!(
            !node.invocation.env.iter().any(|(n, _)| n == k),
            "{k} must not be overlaid on a --live root: {:?}",
            node.invocation.env
        );
    }
    // §6.4's MUST is unchanged by live mode: the declaration is still marion's, inside the
    // node's own agent dir, and it is still what argv names.
    let declaration = node.mcp_config.as_ref().unwrap();
    assert!(declaration.is_file());
    assert!(declaration.starts_with(node.agent_dir.path()));
    assert!(
        node.invocation
            .args
            .iter()
            .any(|a| a == "--strict-mcp-config")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// **§6.4's central MUST at the one place `--live` could break it on codex: marion must never
/// mutate the operator's own harness config.**
///
/// Unsetting `CODEX_HOME` is what makes the operator's `~/.codex/auth.json` visible — and it
/// makes `~/.codex/config.toml` the *only* config codex will read. `prepare` writes whatever
/// `config_files` returns, unconditionally and with `create_dir_all` on the parent, so an
/// adapter that kept emitting a document here would have marion write over the operator's real
/// codex config, and the first symptom would be a broken login on a harness marion was not even
/// running. Asserted three ways: nothing was written, `CODEX_HOME` is absent by name, and the
/// declaration is on argv where [`McpRoute::Argv`]'s verification above found it.
#[test]
fn a_live_codex_root_writes_no_file_and_so_cannot_touch_the_operators_own_codex_config() {
    let dir = temp("live-codex");
    let node = prepare(&RootSpec {
        base_url: None,
        auth: Auth::Inherited,
        ..root_spec(&dir, "codex")
    })
    .unwrap();
    assert!(
        node.mcp_config.is_none(),
        "the live route is argv, so there is no document to name: {:?}",
        node.mcp_config
    );
    assert!(
        std::fs::read_dir(node.agent_dir.config_dir())
            .unwrap()
            .next()
            .is_none(),
        "marion wrote into {} on a route whose only readable config.toml is ~/.codex/config.toml",
        node.agent_dir.config_dir().display()
    );
    for k in ["CODEX_HOME", "MARION_PROVIDER_KEY"] {
        assert!(
            !node.invocation.env.iter().any(|(n, _)| n == k),
            "{k} must be absent, not blank: {:?}",
            node.invocation.env
        );
    }
    // The declaration `McpRoute::Argv` promised, actually present — including the two settings
    // whose absence is silent (§12): the approval mode and the plugin fetch.
    let joined = node.invocation.args.join(" ");
    for needle in [
        "-c mcp_servers.marion.command=",
        r#"-c mcp_servers.marion.default_tools_approval_mode="approve""#,
        "-c features.plugins=false",
        r#"-c mcp_servers.marion.env.MARION_DEPTH="0""#,
        r#"-c mcp_servers.marion.env.MARION_AUTH="inherited""#,
    ] {
        assert!(
            joined.contains(needle),
            "missing `{needle}` from:\n{joined}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A prepared root never debug-prints its run token** — neither the field that holds it nor
/// the compiled environment it reaches as `ANTHROPIC_AUTH_TOKEN`. A `RootNode` is what a
/// launch failure has in hand, so its `{:?}` is the one most likely to reach an error.
#[test]
fn a_prepared_roots_debug_form_carries_no_run_token() {
    let dir = temp("root-debug");
    let node = prepare(&root_spec(&dir, "claude-orchestrator")).unwrap();
    let token = node
        .invocation
        .env
        .iter()
        .find(|(k, _)| k == "ANTHROPIC_AUTH_TOKEN")
        .map(|(_, v)| v.clone())
        .expect("the token still reaches the variable the harness reads");
    assert!(token.starts_with("marion-run-"), "{}", token.len());
    let printed = format!("{node:?} {node:#?}");
    assert!(!printed.contains(&token), "{printed}");
    assert!(printed.contains("ANTHROPIC_AUTH_TOKEN"), "{printed}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The canned root is untouched by the axis existing — the pair is still compiled.
#[test]
fn a_canned_root_still_carries_the_pair_exactly_where_it_always_did() {
    let dir = temp("canned-auth");
    let node = prepare(&root_spec(&dir, "claude-orchestrator")).unwrap();
    let get = |k: &str| {
        node.invocation
            .env
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(
        get("ANTHROPIC_AUTH_TOKEN"),
        Some(node.token.expose().to_string())
    );
    assert_eq!(get("ANTHROPIC_API_KEY"), Some(String::new()));
    assert_eq!(
        get("ANTHROPIC_BASE_URL"),
        Some("http://127.0.0.1:8099".into())
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_agent_type_nobody_declared_is_refused_by_name_before_anything_is_written() {
    let dir = temp("unknown");
    let mut spec = root_spec(&dir, "claude");
    spec.agent_type = "not-a-harness".into();
    spec.model = None;
    assert!(matches!(
        prepare(&spec),
        Err(RootError::UnknownAgentType(t)) if t == "not-a-harness"
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A run whose marion calls all came back **answered** — the only shape that satisfies the gate,
/// so every case below states its own departure from it rather than inheriting one.
fn ran(calls: &[&str], frames: usize, exit: Option<i32>, stderr: &str) -> RootOutcome {
    outcome(
        &calls
            .iter()
            .map(|v| (*v, CallOutcome::Answered))
            .collect::<Vec<_>>(),
        frames,
        exit,
        stderr,
    )
}

fn outcome(
    calls: &[(&str, CallOutcome)],
    frames: usize,
    exit: Option<i32>,
    stderr: &str,
) -> RootOutcome {
    RootOutcome {
        exit_code: exit,
        transcript: vec![json!({}); frames],
        stderr: stderr.to_string(),
        marion_calls: calls
            .iter()
            .map(|(verb, outcome)| MarionCall {
                verb: (*verb).to_string(),
                outcome: outcome.clone(),
            })
            .collect(),
        ..RootOutcome::default()
    }
}

/// **A root that answered plainly and exited cleanly is a normal run, not a refusal.**
///
/// Measured live: `marion run codex --prompt "say hello"` printed "Hello!" and marion exited 1
/// with "the root never reached marion's bridge". A prompt that needs no delegation is a
/// legitimate use, so the run is `Ok` and carries a note saying nothing was delegated — the
/// note is what keeps it from being the silent success §6.1 step 8 refuses.
#[test]
fn a_launch_only_root_that_answered_without_marion_is_ok_with_a_note() {
    let answered = ran(&[], 3, Some(0), "");
    assert!(assert_a_verb_was_answered(Harness::Codex, &answered).is_ok());
    let (status, exit) = roots_exit(&RootOutcome {
        bridge_unused: true,
        ended: None,
        ..answered
    });
    assert_eq!(status, ExitStatus::Ok);
    assert!(
        exit.description.contains(ANSWERED_WITHOUT_DELEGATING),
        "{}",
        exit.description
    );
    // A root that did call marion gets no such note.
    let (_, delegated) = roots_exit(&ran(&["spawn"], 3, Some(0), ""));
    assert!(!delegated.description.contains(ANSWERED_WITHOUT_DELEGATING));
}

/// **A held boot dialog is named, then ends the node with the same words.** The attention item
/// and the exit description both carry the harness, the repository and the one action; a node
/// marion ended is `Failed` with that description whatever its exit code.
#[test]
fn a_held_boot_dialog_names_its_action_and_ends_the_node_with_it() {
    let dialog = marion_harness::adapter::harness_spec(Harness::ClaudeCode)
        .boot_dialogs
        .dialogs[0];
    let held = dialog_held("claude", &dialog, "/work/repo");
    assert!(
        held.starts_with("claude is waiting on its boot dialog"),
        "{held}"
    );
    assert!(held.contains("trust /work/repo in claude once"), "{held}");
    assert!(held.contains("worktrees inherit it"), "{held}");
    let why = dialog_expired("claude", &dialog, "/work/repo", 900);
    assert!(why.contains("marion ended the node"), "{why}");
    assert!(why.contains("whole bound (900 s)"), "{why}");
    assert!(why.contains("trust /work/repo in claude once"), "{why}");
    let (status, exit) = roots_exit(&RootOutcome {
        ended: Some(why.clone()),
        ..ran(&[], 3, Some(0), "")
    });
    assert_eq!(status, ExitStatus::Failed);
    assert_eq!(exit.description, why);
}

/// **A worktree's dialog is about its main checkout**, where the harness keys the trust; a
/// directory outside git is about itself.
#[test]
fn a_worktrees_boot_dialog_names_the_main_repository() {
    let dir = marion_testsupport::scratch("root-repository-of");
    let main = dir.join("main");
    let wt = dir.join("wt");
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q", &main.to_string_lossy()]);
    git(&[
        "-C",
        &main.to_string_lossy(),
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "i",
    ]);
    git(&[
        "-C",
        &main.to_string_lossy(),
        "worktree",
        "add",
        "-q",
        &wt.to_string_lossy(),
    ]);
    let canon = main.canonicalize().unwrap().display().to_string();
    assert_eq!(repository_of(&wt), canon);
    assert_eq!(repository_of(&main), canon);
    let plain = dir.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert_eq!(
        repository_of(&plain),
        plain.canonicalize().unwrap().display().to_string()
    );
}

/// **A root killed on its wall clock ends `TimedOut`, whatever else its run says** — the one
/// outcome every root path reports for `marion run --timeout` expiring, the duplex path
/// included now that it runs under the same wall clock (§9). A killed process has no exit code
/// and may leave a stream failure behind; neither outranks marion's own attributed kill.
#[test]
fn a_root_killed_on_its_wall_clock_ends_timed_out() {
    for exit in [None, Some(0), Some(1)] {
        let (status, described) = roots_exit(&RootOutcome {
            timed_out: true,
            failure: Some("the stream ended mid-turn".into()),
            ..ran(&["spawn"], 2, exit, "")
        });
        assert_eq!(status, ExitStatus::TimedOut, "exit {exit:?}");
        assert!(
            described.description.contains("exceeded marion's bound"),
            "{}",
            described.description
        );
    }
}

/// **What still guards a genuinely broken run**: no marion call *and* no answer (no frame at
/// all), a non-zero exit, or a stream that reported a failure is still the loud refusal.
#[test]
fn a_launch_only_root_that_never_reached_the_bridge_and_did_not_answer_is_a_refusal() {
    for (label, o) in [
        ("no frame", ran(&[], 0, Some(0), "")),
        ("non-zero exit", ran(&[], 3, Some(1), "")),
        (
            "stream failure",
            RootOutcome {
                failure: Some("APIError".into()),
                ..ran(&[], 3, Some(0), "")
            },
        ),
    ] {
        let err = assert_a_verb_was_answered(Harness::Codex, &o)
            .expect_err(label)
            .to_string();
        assert!(
            err.contains("never reached marion's bridge"),
            "{label}: {err}"
        );
        assert!(err.starts_with("codex: "), "{label}: {err}");
    }
}

/// **The refusal must carry the harness's own words when the exit code has none.** Measured
/// live: an opencode root against a retired model exits 1 with an **empty stderr** and the whole
/// diagnosis — a 404 naming the model — in a single in-stream `error` frame. Before this, the
/// refusal read `child exit Some(1)` and stopped, which is true and tells the operator nothing
/// about whether the model is gone, the credential is dead, or marion mis-declared the bridge.
/// `spawn::build_contract` has kept this beside a *child's* exit numbers since S13; a root has
/// no `TaskContract`, so the refusal is the only place it can go.
#[test]
fn the_refusal_carries_the_streams_own_failure_when_stderr_is_empty() {
    let silent_but_failed = RootOutcome {
        failure: Some("APIError: model gemini-2.5-flash-lite is no longer available".into()),
        ..ran(&[], 1, Some(1), "")
    };
    let msg = assert_a_verb_was_answered(Harness::OpenCode, &silent_but_failed)
        .expect_err("no marion call is still a refusal")
        .to_string();
    assert!(
        msg.contains("no longer available"),
        "the stream's diagnosis is the only one there is here: {msg}"
    );
    assert!(
        msg.contains("the root's stream reported:"),
        "and it must be labelled as the harness's claim, not marion's: {msg}"
    );
    // The empty case adds no dangling label — the same rule `stderr` already follows.
    let quiet = assert_a_verb_was_answered(Harness::OpenCode, &ran(&[], 1, Some(1), ""))
        .expect_err("still a refusal")
        .to_string();
    assert!(
        !quiet.contains("stream reported"),
        "a stream that claimed nothing must not be quoted as claiming nothing: {quiet}"
    );
}

/// **An authentication failure on stderr is the cause, and the refusal leads with it.**
/// Measured live: a gemini root on a personal login emitted 0 frames and exited 55 with this
/// stderr, and the refusal opened with "the root never reached marion's bridge … 0 frame(s)",
/// which sends the operator to re-check a bridge declaration that was never the problem.
#[test]
fn a_root_that_could_not_authenticate_is_refused_leading_with_the_harnesss_own_words() {
    let line = "Error authenticating: IneligibleTierError: This client is no longer supported \
                for Gemini Code Assist for individuals. To continue using Gemini, please \
                migrate to the Antigravity suite of products";
    let stderr = format!("Loaded cached credentials.\n{line}\n    at async main (gemini.js:1:1)");
    let msg = assert_a_verb_was_answered(Harness::Gemini, &ran(&[], 0, Some(55), &stderr))
        .expect_err("a root that never authenticated delegated nothing")
        .to_string();
    let cause = msg.find(line).expect("the auth line must be quoted");
    let bridge = msg
        .find("never reached marion's bridge")
        .expect("and the bridge sentence kept, after it");
    assert!(
        cause < bridge,
        "the authentication failure is the cause and must come first: {msg}"
    );
    assert!(msg.starts_with("gemini: "), "{msg}");
    // Without an auth line the refusal is unchanged: the bridge sentence leads.
    let plain = assert_a_verb_was_answered(Harness::Gemini, &ran(&[], 0, Some(1), "boom"))
        .expect_err("still a refusal")
        .to_string();
    assert!(
        plain.starts_with("gemini: the root never reached marion's bridge"),
        "{plain}"
    );
}

/// The other half, so the check above cannot pass by always failing: one **answered** call to
/// any marion verb satisfies the gate, whatever the run then did with it.
#[test]
fn one_answered_marion_call_of_any_verb_satisfies_the_post_hoc_assertion() {
    for verb in ["spawn", "status", "wait", "list", "report"] {
        assert!(
            assert_a_verb_was_answered(Harness::Gemini, &ran(&[verb], 1, Some(0), "")).is_ok(),
            "{verb}: the question is whether a verb was answered, not which verb it was"
        );
    }
    // Even a run that then failed: an answered verb and a successful run are different facts,
    // and conflating them would relabel every genuine child failure as a launch failure.
    assert!(
        assert_a_verb_was_answered(Harness::OpenCode, &ran(&["spawn"], 2, Some(1), "it broke"))
            .is_ok()
    );
}

/// **The question the gate now asks, as a table** — because the change is which of these rows
/// is a pass, and a table is where that is legible.
///
/// Row 3 is the defect owed item 0 recorded: a root that reached the bridge, had its one verb
/// refused, and exited 0. Row 5 is why `Unknown` is not folded into an answer: a stream that
/// showed a call and never showed its result is a run that stopped mid-call, and reading it as
/// success is the "passes because it failed to look" shape this repository keeps re-finding.
#[test]
fn the_gate_passes_only_on_an_answered_verb() {
    let refused = || CallOutcome::Refused("§5.4 rejects `report` on a root".into());
    // (label, the run's marion calls, does the gate pass)
    let cases = [
        // A clean exit with frames and no call is a plain answer, passed with a note; see
        // `a_launch_only_root_that_answered_without_marion_is_ok_with_a_note`. The failing
        // no-call shapes are in the test beside that one.
        ("no call at all, a plain answer", vec![], true),
        (
            "one answered call",
            vec![("spawn", CallOutcome::Answered)],
            true,
        ),
        ("one refused call", vec![("spawn", refused())], false),
        (
            "every call refused",
            vec![("report", refused()), ("spawn", refused())],
            false,
        ),
        (
            "a call with no result",
            vec![("spawn", CallOutcome::Unknown)],
            false,
        ),
        (
            "one answered among refusals — the bridge worked, so the run is not refused here",
            vec![
                ("report", refused()),
                ("spawn", CallOutcome::Answered),
                ("status", CallOutcome::Unknown),
            ],
            true,
        ),
    ];
    for (label, calls, ok) in cases {
        assert_eq!(
            assert_a_verb_was_answered(Harness::Gemini, &outcome(&calls, 1, Some(0), "")).is_ok(),
            ok,
            "{label}"
        );
    }
}

/// **Two refusals, because they are two different pieces of news** — the split `36fbbee` made
/// for the bridge's own refusals, applied here.
///
/// A root that never reached the bridge was started wrong and the fix is in the launch. A root
/// whose calls were all refused *had* marion's tools and something turned it away; telling that
/// operator "the root never reached marion's bridge" sends them to re-check a configuration that
/// is working.
/// **A root that started a child delegated, whatever its `spawn` result said.** Measured live
/// (2026-09-22): opencode roots whose child ran, wrote its file and ended `Unreported` got an
/// `isError` spawn result, the stream showed the call refused, and the gate refused the run as
/// "cannot have delegated anything" — about a run with a child and a persisted contract. The
/// journal is the witness: a child of this root whose `Spawned` landed is a spawn marion
/// answered with a node.
#[test]
fn a_root_whose_error_shaped_spawn_started_a_child_delegated() {
    let refused = outcome(
        &[(
            "spawn",
            CallOutcome::Refused(
                "marion: the claude-code child never called report, so it returned no answer"
                    .into(),
            ),
        )],
        2,
        Some(0),
        "",
    );
    assert!(assert_the_root_delegated(Harness::OpenCode, &refused, true).is_ok());
    assert!(matches!(
        assert_the_root_delegated(Harness::OpenCode, &refused, false),
        Err(RootError::NoVerbAnswered { .. })
    ));
}

#[test]
fn a_refused_call_is_not_reported_as_a_bridge_that_was_never_reached() {
    let refused = outcome(
        &[(
            "spawn",
            CallOutcome::Refused("invalid arguments for spawn".into()),
        )],
        2,
        Some(0),
        "",
    );
    let err = assert_a_verb_was_answered(Harness::Gemini, &refused)
        .expect_err("a root whose only verb was refused delegated nothing");
    let msg = err.to_string();
    assert!(matches!(
        err,
        RootError::NoVerbAnswered {
            harness: Harness::Gemini,
            calls: 1,
            exit: Some(0),
            ..
        }
    ));
    assert!(
        !msg.contains("never reached marion's bridge"),
        "it reached the bridge; blaming the launch sends the operator to the wrong fix: {msg}"
    );
    assert!(
        msg.contains("spawn was refused: invalid arguments for spawn"),
        "the refusal must carry the verb AND the refuser's own words: {msg}"
    );
    assert!(
        msg.contains("Some(0)"),
        "and the clean exit code, which is the whole trap: {msg}"
    );

    // The other spelling: a call whose result never arrived says so, rather than claiming a
    // refusal nobody issued.
    let quiet = assert_a_verb_was_answered(
        Harness::Codex,
        &outcome(&[("spawn", CallOutcome::Unknown)], 1, None, ""),
    )
    .expect_err("no answer is not an answer")
    .to_string();
    assert!(
        quiet.contains("never showed a result") && !quiet.contains("was refused"),
        "a stream that showed no result must not be reported as a rule violation: {quiet}"
    );
}

/// The refusal carries the child's own words when it had any — S13 measured an opencode failure
/// arriving with an **empty** stderr, so the label must not be printed when there is nothing
/// behind it.
#[test]
fn the_refusal_quotes_stderr_only_when_there_is_some() {
    let with = assert_a_verb_was_answered(
        Harness::OpenCode,
        &ran(&[], 0, None, "  no provider configured\n"),
    )
    .unwrap_err()
    .to_string();
    assert!(with.contains("stderr: no provider configured"), "{with}");
    let without = assert_a_verb_was_answered(Harness::OpenCode, &ran(&[], 0, None, "  \n"))
        .unwrap_err()
        .to_string();
    assert!(!without.contains("stderr:"), "{without}");
}
