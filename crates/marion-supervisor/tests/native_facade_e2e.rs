//! **M3's native facade, end to end and harness-generic** (`plan-native-activation.md` step 9).
//!
//! For every lane `production_native_facades().enabled_native_commands()` names, the **shipped**
//! `marion <harness>` binary runs on its own controlling PTY against a supervisor composed the way
//! `detach.rs` stage 3 composes it, in front of the harness's real TUI at its first screen — a
//! native node is the operator's own login, so the tail asks nothing of a provider. What is
//! asserted is causal and read where each effect lands: the operator's screen through
//! `marion_term`, the node's `pty.cast`, the journal, the client's wait status and termios.
//!
//! Two clauses of step 9 are stated here rather than asserted, because the fixture cannot observe
//! them and a green that could not fail is not evidence:
//!
//! * **Re-attach through the facade is impossible (single-use capability).** The facade has no
//!   attach verb and the client's capability dies with its process, so there is nothing for a test
//!   outside that process to replay. The single-use property is proved at the boundary that mints
//!   it (`tests/native_bootstrap.rs`, `native_bootstrap::PreparedNativeLaunchClaim`); what this
//!   file proves is the consequence — after a detach the tree still holds exactly one node.
//! * **Signal actions default in a probe child.** The relay restores its prior actions inside the
//!   client process, which then exits; no child of that process exists to inherit them. The
//!   observable consequence is asserted instead: `SIGTERM` ends the client **by that signal**
//!   (`native_facade_sigterm_restores_the_operator_terminal_and_exits_by_signal`), which is only
//!   possible once `SIG_DFL` is back and the signal unblocked.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_core::paths::ProjectDir;
use marion_supervisor::handler::RegistryHandle;
use marion_supervisor::pty::{PtyHost, PtyMaster, StdinPlan, WinSize, spawn_pty};
use marion_supervisor::registry::{LiveRegistry, Registry};
use marion_supervisor::serve::{NativeLaunchConfig, Server, own_uid};
use marion_supervisor::socket::{Acquired, SocketPaths, acquire, project_root, socket_paths};
use marion_testsupport::{on_path, scratch};

mod common;
use common::client::Client;

/// Every wait in this file is bounded by this and none is a verdict.
const BOUND: Duration = Duration::from_secs(90);

/// The operator's terminal as the client finds it.
const OPERATOR_SIZE: WinSize = WinSize {
    cols: 117,
    rows: 43,
};

/// Where the operator drags the corner to.
const RESIZED: WinSize = WinSize {
    cols: 101,
    rows: 31,
};

/// `SIGTERM` on both supported targets.
const SIGTERM: i32 = 15;

/// Keys a first screen may answer, tried in order: an arrow (a dialog moves its selection), then a
/// letter (a composer echoes it). Neither submits anything.
const KEYS: &[&[u8]] = &[b"\x1b[B", b"x"];

/// How long a key gets to change the node's output before it is presumed swallowed and re-sent
/// (`pane_attach.rs::KEY_SETTLE`).
const KEY_SETTLE: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------------------------
// The supervisor, composed as `detach::run_stage_three` composes it
// ---------------------------------------------------------------------------------------------

struct Bed {
    /// Keeps the scratch tree alive for the bed's life; dropping it sweeps the tree.
    _work: marion_testsupport::Scratch,
    state: PathBuf,
    project: PathBuf,
    project_dir: ProjectDir,
    paths: SocketPaths,
    server: Option<Server>,
}

impl Bed {
    fn new(tag: &str) -> Bed {
        let work = scratch(tag);
        let state = work.join("state");
        let project = work.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let key = project_root(&project);
        let paths = socket_paths(&state, &key, own_uid());
        assert!(
            paths.overflow().is_none(),
            "this bed's socket must live under <state>; {:?} overflowed to /tmp",
            paths.socket()
        );
        let Acquired::Serving(serving) = acquire(&paths).expect("bind both listeners") else {
            panic!("fresh project unexpectedly dialed an existing supervisor")
        };
        let project_dir = ProjectDir::new(&state, paths.canonical_project());
        let live = Arc::new(LiveRegistry::follow(
            Registry::boot(&project_dir).expect("an absent journal is an empty tree"),
            Duration::from_millis(10),
        ));
        let env = marion_supervisor::run::Env {
            project_dir: project_dir.clone(),
            state: state.clone(),
            project_root: key.clone(),
            bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
            // Canned: a native node is the operator's own login and never dials this.
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: marion_harness::Auth::Canned,
        };
        let handle = RegistryHandle::owning(live, env.clone());
        let server = Server::start_with_native_launch(
            serving,
            Arc::clone(&handle),
            NativeLaunchConfig {
                descriptors: marion_core::PRODUCTION_NATIVE_FACADES,
                adapter_for: marion_harness::native_adapter,
                env,
            },
            Duration::from_secs(300),
        )
        .expect("the enabled native bootstrap service installs once");
        Bed {
            _work: work,
            state,
            project,
            project_dir,
            paths,
            server: Some(server),
        }
    }

    /// The one native root this bed's supervisor spawned, once the journal names it.
    fn native_root(&self) -> AgentId {
        let mut found = None;
        assert!(
            until(|| {
                let replayed = Registry::boot(&self.project_dir).expect("the journal replays");
                found = replayed
                    .tree()
                    .nodes()
                    .iter()
                    .find(|node| node.spawn_confirmed)
                    .map(|node| node.agent_id.clone());
                found.is_some()
            }),
            "the supervisor never journaled a Spawned native root"
        );
        found.expect("a confirmed native root")
    }

    fn node_cast(&self, agent: &AgentId) -> PathBuf {
        self.project_dir.agent(agent).pty_cast()
    }

    fn replayed_nodes(&self) -> Vec<marion_core::registry::ReplayedNode> {
        Registry::boot(&self.project_dir)
            .expect("the journal replays")
            .tree()
            .nodes()
            .to_vec()
    }

    /// The supervisor ends the node (§7.3.2 `KillTree`), and the journal says so.
    fn kill_and_await_exit(&self, agent: &AgentId) -> marion_core::registry::ReplayedNode {
        let mut client = Client::dial(&self.paths);
        let id = client.send(marion_proto::Call::SessionQuit(
            marion_proto::params::SessionQuitParams {
                disposition: marion_proto::QuitDisposition::KillTree {
                    confirmed: vec![agent.clone()],
                },
            },
        ));
        let (_, outcome) = client.read_to_response(id);
        let marion_proto::Outcome::Result(body) = outcome else {
            panic!("session/quit KillTree was refused: {outcome:?}")
        };
        let Ok(marion_proto::MethodResult::SessionQuit(result)) =
            marion_proto::Method::SessionQuit.decode_result(&body)
        else {
            panic!("session/quit answered with the wrong result: {body:?}")
        };
        assert!(
            matches!(result.outcome, marion_proto::QuitOutcome::Killed { .. }),
            "KillTree did not kill: {:?}",
            result.outcome
        );
        // The lifecycle worker journals `Exited` after it reaps; wait on the journal, which is the
        // barrier the supervisor itself exits on (`native_bootstrap.rs`).
        let mut node = None;
        assert!(
            until(|| {
                node = self
                    .replayed_nodes()
                    .into_iter()
                    .find(|n| &n.agent_id == agent && n.exit.is_some());
                node.is_some()
            }),
            "no Exited record followed the confirmed kill: {:?}",
            self.replayed_nodes()
        );
        node.expect("an exited node")
    }
}

impl Drop for Bed {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.stop();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The operator's terminal, with the shipped facade on it
// ---------------------------------------------------------------------------------------------

/// The terminal's line discipline, read through a slave opened for the read and closed again.
fn termios_of(master: &PtyMaster) -> String {
    let probe: OwnedFd = master.open_slave().expect("termios probe slave");
    format!("{:?}", rustix::termios::tcgetattr(probe.as_fd()).unwrap())
}

struct Operator {
    host: PtyHost,
    cast: PathBuf,
    baseline: String,
}

impl Operator {
    /// `marion <harness> <tail>` as a foreground session leader on a fresh PTY marion records, so
    /// the operator's screen can be read back through `marion_term` exactly as `pane_attach.rs`
    /// reads it. `spawn_pty` performs the `setsid` + `TIOCSCTTY` the facade's controlling-TTY
    /// witness requires.
    fn facade(bed: &Bed, harness: &str, tail: &[&str]) -> Operator {
        let cast = bed.state.join(format!("operator-{harness}.cast"));
        let master = PtyMaster::open(OPERATOR_SIZE).expect("the operator's pty");
        // The baseline is read through a slave held open until the client owns one: a master whose
        // only slave has been opened and closed reads EOF, which would end the host's reader thread
        // before the client ever wrote to it.
        let probe: OwnedFd = master.open_slave().expect("termios baseline slave");
        let baseline = format!("{:?}", rustix::termios::tcgetattr(probe.as_fd()).unwrap());
        let host = PtyHost::start(
            AgentId("operator".into()),
            master,
            &cast,
            OPERATOR_SIZE,
            "xterm-256color",
            Instant::now(),
        )
        .expect("recording the operator's screen");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_marion"));
        cmd.arg(harness)
            .args(tail)
            .current_dir(&bed.project)
            .env("MARION_STATE_DIR", &bed.state)
            .env("TERM", "xterm-256color");
        let witness = marion_harness::ExecutionSurfaces::opaque()
            .display_plane()
            .expect("the opaque shape declares a display plane");
        let child = spawn_pty(
            witness,
            &mut cmd,
            host.master(),
            StdinPlan::TerminalSlave,
            None,
        )
        .expect("the shipped marion binary starts on the operator's pty");
        host.adopt(child);
        drop(probe);
        Operator {
            host,
            cast,
            baseline,
        }
    }

    /// What the operator can read: the recorded bytes put back through a terminal emulator.
    fn screen(&self) -> String {
        let mut term = marion_term::Term::with_options(
            marion_term::Size::new(OPERATOR_SIZE.cols as usize, OPERATOR_SIZE.rows as usize),
            marion_tui::grid_options(),
        );
        for (code, data) in cast_records(&self.cast) {
            match code.as_str() {
                "o" => term.advance(data.as_bytes()),
                "r" => resize_from(&mut term, &data),
                _ => {}
            }
        }
        let mut lines = term.scrollback_lines();
        lines.extend(term.viewport_lines());
        lines.join("\n")
    }

    fn type_in(&self, bytes: &[u8]) {
        if let Err(error) = self.host.master().write_all(bytes) {
            panic!(
                "writing {bytes:?} to the operator's terminal: {error}; client exited={}; what the \
                 client last wrote:\n{}",
                self.exited(),
                tail(&cast_text(&self.cast, "o"))
            );
        }
    }

    /// `TIOCSWINSZ` on the operator's master; the relay's geometry tick is what forwards it.
    fn resize_window(&self, size: WinSize) {
        self.host
            .master()
            .set_size(size)
            .expect("resizing the operator's terminal");
    }

    fn pid(&self) -> i32 {
        self.host.child_pid().expect("the client was adopted")
    }

    fn exited(&self) -> bool {
        self.host
            .poll_exited_unreaped()
            .expect("polling the shipped client")
    }

    fn termios(&self) -> String {
        termios_of(self.host.master())
    }

    /// Reap the client (killing it first if it is still there) and return its wait status.
    fn finish(self) -> Option<std::process::ExitStatus> {
        self.host
            .shutdown()
            .expect("shutting the operator's pty down")
    }
}

// ---------------------------------------------------------------------------------------------
// Reading a cast
// ---------------------------------------------------------------------------------------------

fn cast_records(path: &Path) -> Vec<(String, String)> {
    let Ok(s) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    s.lines()
        .skip(1)
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            let a = v.as_array()?;
            Some((
                a.get(1)?.as_str()?.to_string(),
                a.get(2)?.as_str()?.to_string(),
            ))
        })
        .collect()
}

fn cast_text(path: &Path, code: &str) -> String {
    cast_records(path)
        .into_iter()
        .filter(|(c, _)| c == code)
        .map(|(_, d)| d)
        .collect()
}

fn geometries(path: &Path) -> Vec<String> {
    cast_records(path)
        .into_iter()
        .filter(|(c, _)| c == "r")
        .map(|(_, d)| d)
        .collect()
}

fn resize_from(term: &mut marion_term::Term, record: &str) {
    if let Some((c, r)) = record.split_once('x')
        && let (Ok(c), Ok(r)) = (c.parse(), r.parse())
    {
        term.resize(marion_term::Size::new(c, r));
    }
}

fn tail(s: &str) -> String {
    let mut n = s.len().saturating_sub(1500);
    while !s.is_char_boundary(n) {
        n += 1;
    }
    s[n..].escape_debug().to_string()
}

fn until(mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + BOUND;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

/// A word the harness painted: the longest run of letters on the operator's screen that the
/// node's own recorded output also carries. Marion paints no words of its own on this path, so a
/// word on both sides is a cell of the harness's TUI rendered through the relay.
fn harness_word_on_screen(screen: &str, node_output: &str) -> Option<String> {
    let mut words: Vec<&str> = screen
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| w.len() >= 5)
        .collect();
    words.sort_by_key(|w| std::cmp::Reverse(w.len()));
    words
        .into_iter()
        .find(|w| node_output.contains(w))
        .map(str::to_string)
}

/// The client left before the test asked it to: report its wait status, its last words on the
/// operator's terminal and the journal's view of the node, which together name the cause.
fn unexpected_client_exit(op: Operator, bed: &Bed, when: &str) -> ! {
    let last_written = tail(&cast_text(&op.cast, "o"));
    let status = op.finish();
    panic!(
        "{when} the client exited on its own: status {status:?}; what it last wrote:\n\
         {last_written}\njournal: {:?}",
        bed.replayed_nodes()
    );
}

/// Run one enabled lane up to its first screen, or `None` with a loud skip if the harness is not
/// installed. Every other clause builds on the returned pair.
fn first_screen(bed: &Bed, harness: &str) -> Option<(Operator, AgentId)> {
    if !on_path(harness) {
        eprintln!("SKIP: `{harness}` is not on PATH, so its native facade E2E did not run");
        return None;
    }
    let op = Operator::facade(bed, harness, &[]);
    let agent = bed.native_root();
    let node_cast = bed.node_cast(&agent);
    assert!(
        until(|| node_cast.exists()),
        "[{harness}] the native node has no pty recording at {}",
        node_cast.display()
    );
    let mut word = None;
    let painted = until(|| {
        word = harness_word_on_screen(&op.screen(), &cast_text(&node_cast, "o"));
        word.is_some() || op.exited()
    });
    if op.exited() {
        unexpected_client_exit(
            op,
            bed,
            &format!("[{harness}] before its first screen was read"),
        );
    }
    assert!(
        painted,
        "[{harness}] no cell of the harness's TUI reached the operator's screen. What the operator \
         saw:\n{}\nraw operator bytes ({}): {:?}\nnode output ({} bytes): {:?}",
        op.screen(),
        cast_text(&op.cast, "o").len(),
        tail(&cast_text(&op.cast, "o")),
        cast_text(&node_cast, "o").len(),
        tail(&cast_text(&node_cast, "o")),
    );
    eprintln!(
        "[{harness}] first screen: the word {:?} is on the operator's screen and in the node's output",
        word.unwrap()
    );
    Some((op, agent))
}

// ---------------------------------------------------------------------------------------------
// Step 9: every enabled lane, one run each
// ---------------------------------------------------------------------------------------------

#[test]
fn every_enabled_native_lane_runs_its_real_tui_through_the_shipped_facade() {
    let registry = marion_core::production_native_facades();
    let lanes = registry.enabled_native_commands();
    assert!(
        !lanes.is_empty(),
        "no native lane is enabled, so this matrix is empty"
    );
    for harness in lanes {
        let bed = Bed::new(&format!("native-e2e-{harness}"));
        let Some((op, agent)) = first_screen(&bed, harness) else {
            continue;
        };
        let node_cast = bed.node_cast(&agent);

        // ---- a keystroke reaches the node, and marion keeps it opaque ----
        //
        // The far end is the node's recording: it gains output after the keystroke (the harness
        // redrew), and it gains **no `i` record** — a native pane's input is never journaled, which
        // is the opacity invariant, asserted where a regression would land it.
        //
        // Which key a first screen answers is the harness's business (claude's trust dialog moves
        // on an arrow and ignores letters; codex's composer echoes letters), so each candidate is
        // sent in turn and the first the node redrew for is the keystroke that arrived. A key the
        // TUI swallowed is re-sent only after the node has been still for a settle period,
        // `pane_attach.rs::type_until`'s rule.
        let before = cast_text(&node_cast, "o").len();
        let mut sent = Vec::new();
        let landed = 'keys: {
            for _ in 0..3 {
                for key in KEYS {
                    op.type_in(key);
                    sent.push(*key);
                    let settle = Instant::now() + KEY_SETTLE;
                    while Instant::now() < settle {
                        if cast_text(&node_cast, "o").len() > before {
                            break 'keys true;
                        }
                        if op.exited() {
                            break 'keys false;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
            false
        };
        if op.exited() {
            unexpected_client_exit(op, &bed, &format!("[{harness}] after {sent:?} was typed"));
        }
        assert!(
            landed,
            "[{harness}] the node produced no output after {} keystrokes ({sent:?}); its recording \
             is unchanged at {before} bytes",
            sent.len()
        );
        assert!(
            cast_text(&node_cast, "i").is_empty(),
            "[{harness}] the native node's recording carries an `i` record, so its input went \
             through the legacy pane write rather than the opaque native path: {:?}",
            cast_text(&node_cast, "i")
        );

        // ---- a resize reaches the one master ----
        let want = RESIZED.as_cast();
        op.resize_window(RESIZED);
        assert!(
            until(|| geometries(&node_cast).contains(&want)),
            "[{harness}] the node's pty was never resized to {want}; its geometries were {:?}",
            geometries(&node_cast)
        );
        assert!(
            geometries(&node_cast).len() >= 2,
            "[{harness}] the node only ever had one geometry ({:?})",
            geometries(&node_cast)
        );

        // ---- detach: the client leaves, the node stays ----
        op.type_in(&[marion_tui::keys::PREFIX, marion_tui::keys::DETACH_KEY]);
        assert!(
            until(|| op.exited()),
            "[{harness}] `marion {harness}` did not exit on ^]d"
        );
        let restored = op.termios();
        let baseline = op.baseline.clone();
        let last_written = tail(&cast_text(&op.cast, "o"));
        let status = op.finish().expect("the client's wait status");
        assert!(
            status.success(),
            "[{harness}] a detach is a clean exit, got {status}; what the client last wrote:\n\
             {last_written}"
        );
        assert_eq!(
            restored, baseline,
            "[{harness}] the operator's terminal was not restored after the detach"
        );
        let nodes = bed.replayed_nodes();
        assert_eq!(
            nodes.len(),
            1,
            "[{harness}] the facade minted more than one node: {nodes:?}"
        );
        assert!(
            nodes[0].exit.is_none(),
            "[{harness}] the node ended when its client detached: {:?}",
            nodes[0].exit
        );

        // ---- the supervisor ends it, and the record says the process was alive to be ended ----
        //
        // `Exited` carrying marion's own SIGKILL is positive evidence the node survived the detach:
        // a node that had died with its client would have been recorded with its own exit first.
        let node = bed.kill_and_await_exit(&agent);
        assert!(node.state.is_exited(), "[{harness}] {:?}", node.state);
        assert_eq!(
            node.exit.as_ref().and_then(|exit| exit.signal),
            Some(9),
            "[{harness}] the node did not die by the supervisor's kill: {:?}",
            node.exit
        );
        eprintln!("[{harness}] native facade E2E: first screen, keystroke, resize, detach, kill");
    }
}

// ---------------------------------------------------------------------------------------------
// Step 10: SIGTERM at the client
// ---------------------------------------------------------------------------------------------

/// `docs/superpowers/specs/2026-08-13-native-relay-synchronous-signals-design.md`: an owned
/// termination restores the terminal, then delivers the **selected default** so the client dies by
/// that signal; marion neither kills nor signals the node, which survives for ordinary reconnect.
#[test]
fn native_facade_sigterm_restores_the_operator_terminal_and_exits_by_signal() {
    use std::os::unix::process::ExitStatusExt as _;
    let registry = marion_core::production_native_facades();
    for harness in registry.enabled_native_commands() {
        let bed = Bed::new(&format!("native-e2e-sigterm-{harness}"));
        let Some((op, agent)) = first_screen(&bed, harness) else {
            continue;
        };
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        // SAFETY: `pid` is the exact adopted client, still unreaped by the host.
        assert_eq!(unsafe { kill(op.pid(), SIGTERM) }, 0);
        assert!(
            until(|| op.exited()),
            "[{harness}] the client did not exit after SIGTERM"
        );
        let restored = op.termios();
        let baseline = op.baseline.clone();
        let status = op.finish().expect("the client's wait status");
        assert_eq!(
            status.signal(),
            Some(SIGTERM),
            "[{harness}] the client did not die by SIGTERM's default action: {status}"
        );
        assert_eq!(
            restored, baseline,
            "[{harness}] the operator's terminal was not restored before the default delivery"
        );
        // The node was neither killed nor signalled, and is alive to be ended by the supervisor.
        let nodes = bed.replayed_nodes();
        assert!(
            nodes.len() == 1 && nodes[0].exit.is_none(),
            "[{harness}] the node did not survive the client's termination: {nodes:?}"
        );
        let node = bed.kill_and_await_exit(&agent);
        assert_eq!(
            node.exit.as_ref().and_then(|exit| exit.signal),
            Some(9),
            "[{harness}] the node was not alive for the supervisor to end: {:?}",
            node.exit
        );
    }
}
