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
use marion_supervisor::serve::{NativeLaunchConfig, Server};
use marion_supervisor::socket::own_uid;
use marion_supervisor::socket::{Acquired, SocketPaths, acquire, project_root, socket_paths};
use marion_testsupport::{on_path, scratch, until_within};

mod common;
use common::cast::{cast_records, cast_text};
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
        // Canned: a native node is the operator's own login and never dials this.
        Bed::serving(tag, "http://127.0.0.1:8099/v1", false)
    }

    /// A bed whose supervisor compiles `base_url` into the declarations of the children it spawns.
    /// `repo`: the project is a one-commit repository, for a run whose native root delegates — a
    /// child gets a §6.6 worktree of it.
    fn serving(tag: &str, base_url: &str, repo: bool) -> Bed {
        let work = scratch(tag);
        let state = work.join("state");
        let project = if repo {
            marion_testsupport::fixture_repo(&work)
        } else {
            let project = work.join("project");
            std::fs::create_dir_all(&project).unwrap();
            project
        };
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
            base_url: Some(base_url.into()),
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
        let id = client.send(marion_core::proto::Call::SessionQuit(
            marion_core::proto::params::SessionQuitParams {
                disposition: marion_core::proto::QuitDisposition::KillTree {
                    confirmed: vec![agent.clone()],
                },
            },
        ));
        let (_, outcome) = client.read_to_response(id);
        let marion_core::proto::Outcome::Result(body) = outcome else {
            panic!("session/quit KillTree was refused: {outcome:?}")
        };
        let Ok(marion_core::proto::MethodResult::SessionQuit(result)) =
            marion_core::proto::Method::SessionQuit.decode_result(&body)
        else {
            panic!("session/quit answered with the wrong result: {body:?}")
        };
        assert!(
            matches!(
                result.outcome,
                marion_core::proto::QuitOutcome::Killed { .. }
            ),
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
    /// The operator's window as last set. `resize_window` is a raw `TIOCSWINSZ`, which the cast
    /// does not record, so the bottom row of the terminal is known here and nowhere in the cast.
    size: std::cell::Cell<WinSize>,
    /// Each resize, with how many cast records existed when it was made, so a replay can resize at
    /// about the same point in the stream the terminal did. Replaying everything at the final size
    /// instead wraps the pre-resize output of a main-screen TUI differently from the node's own
    /// screen, and the two can then never be compared.
    resizes: std::cell::RefCell<Vec<(usize, WinSize)>>,
}

impl Operator {
    /// `marion <harness> <tail>` as a foreground session leader on a fresh PTY marion records, so
    /// the operator's screen can be read back through `marion_term` exactly as `pane_attach.rs`
    /// reads it. `spawn_pty` performs the `setsid` + `TIOCSCTTY` the facade's controlling-TTY
    /// witness requires.
    ///
    /// `env` is set on the client, which is how it reaches the node: a native node's environment
    /// is the operator's (`assemble_native` carries the client's minus `MARION_*`).
    fn facade(bed: &Bed, harness: &str, flags: &[String], env: &[(String, String)]) -> Operator {
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
            .args(flags)
            .current_dir(&bed.project)
            .env("MARION_STATE_DIR", &bed.state)
            .env("TERM", "xterm-256color");
        for (key, value) in env {
            cmd.env(key, value);
        }
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
            size: std::cell::Cell::new(OPERATOR_SIZE),
            resizes: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// The recorded bytes put back through a terminal emulator of `size`, cast resizes included.
    fn term_at(&self, size: WinSize) -> marion_term::Term {
        let mut term = marion_term::Term::with_options(
            marion_term::Size::new(size.cols as usize, size.rows as usize),
            marion_tui::grid_options(),
        );
        for (code, data) in cast_records(&self.cast) {
            match code.as_str() {
                "o" => term.advance(data.as_bytes()),
                "r" => resize_from(&mut term, &data),
                _ => {}
            }
        }
        term
    }

    /// The terminal as the operator has it now: the recording replayed from the original size,
    /// resized where [`Self::resize_window`] resized it.
    fn live_term(&self) -> marion_term::Term {
        let mut term = marion_term::Term::with_options(
            marion_term::Size::new(OPERATOR_SIZE.cols as usize, OPERATOR_SIZE.rows as usize),
            marion_tui::grid_options(),
        );
        let resizes = self.resizes.borrow();
        let mut pending = resizes.iter().peekable();
        for (index, (code, data)) in cast_records(&self.cast).into_iter().enumerate() {
            while let Some((_, size)) = pending.next_if(|(at, _)| *at <= index) {
                term.resize(marion_term::Size::new(
                    size.cols as usize,
                    size.rows as usize,
                ));
            }
            match code.as_str() {
                "o" => term.advance(data.as_bytes()),
                "r" => resize_from(&mut term, &data),
                _ => {}
            }
        }
        for (_, size) in pending {
            term.resize(marion_term::Size::new(
                size.cols as usize,
                size.rows as usize,
            ));
        }
        term
    }

    /// What the operator can read: scrollback and viewport together.
    fn screen(&self) -> String {
        let term = self.term_at(OPERATOR_SIZE);
        let mut lines = term.scrollback_lines();
        lines.extend(term.viewport_lines());
        lines.join("\n")
    }

    /// The bottom row of the operator's terminal **at its current size**, where marion's status
    /// line goes, from [`Self::live_term`].
    fn last_viewport_line(&self) -> String {
        self.live_term().viewport_lines().pop().unwrap_or_default()
    }

    /// Every row of the operator's terminal but the last, at its current size.
    fn rows_above_the_last(&self) -> Vec<String> {
        let mut lines = self.live_term().viewport_lines();
        lines.pop();
        lines
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
        self.resizes
            .borrow_mut()
            .push((cast_records(&self.cast).len(), size));
        self.host
            .master()
            .set_size(size)
            .expect("resizing the operator's terminal");
        self.size.set(size);
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

fn geometries(path: &Path) -> Vec<String> {
    cast_records(path)
        .into_iter()
        .filter(|(c, _)| c == "r")
        .map(|(_, d)| d)
        .collect()
}

/// The node's screen as the node itself drew it: its own recording, replayed through every
/// geometry the node's pty was given (the first record is its opening size).
fn node_screen(node_cast: &Path) -> Vec<String> {
    let mut term = marion_term::Term::with_options(
        marion_term::Size::new(OPERATOR_SIZE.cols as usize, OPERATOR_SIZE.rows as usize),
        marion_tui::grid_options(),
    );
    for (code, data) in cast_records(node_cast) {
        match code.as_str() {
            "o" => term.advance(data.as_bytes()),
            "r" => resize_from(&mut term, &data),
            _ => {}
        }
    }
    term.viewport_lines()
}

/// Whether the last alternate-screen switch in `output` entered it.
fn on_alternate_screen(output: &str) -> bool {
    match (output.rfind("\u{1b}[?1049h"), output.rfind("\u{1b}[?1049l")) {
        (Some(on), Some(off)) => on > off,
        (on, _) => on.is_some(),
    }
}

/// The operator's stream with marion's own bytes taken out: the status row
/// (`ESC 7 CSI 1;limit r CSI row;1H CSI 2K CSI 7m … CSI 0m ESC 8`), its clear
/// (`ESC 7 CSI r CSI row;1H CSI 2K ESC 8`), and a scroll-region repair (`CSI top;limit r`, or the
/// same between `ESC 7`/`ESC 8`) where it directly follows a node sequence it repairs.
fn without_marions_bytes(operator: &str, limit: u16) -> String {
    let row = format!(
        "\u{1b}7\u{1b}[1;{limit}r\u{1b}[{};1H\u{1b}[2K\u{1b}[7m",
        limit + 1
    );
    let clear = format!("\u{1b}7\u{1b}[r\u{1b}[{};1H\u{1b}[2K\u{1b}8", limit + 1);
    let saved_repair = format!("\u{1b}7\u{1b}[1;{limit}r\u{1b}8");
    let repair_tail = format!(";{limit}r");
    let mut out = String::with_capacity(operator.len());
    let mut rest = operator;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix(row.as_str()) {
            // A row inside a node sequence would strip back to the node's bytes and hide the very
            // corruption this looks for, so it is made visible instead.
            if ends_inside_a_csi(&out) {
                out.push_str("<marion's row inside a node CSI>");
            }
            let end = after
                .find("\u{1b}[0m\u{1b}8")
                .map_or(after.len(), |end| end + 6);
            rest = &after[end..];
        } else if let Some(after) = rest.strip_prefix(clear.as_str()) {
            if ends_inside_a_csi(&out) {
                out.push_str("<marion's clear inside a node CSI>");
            }
            rest = after;
        } else if out.ends_with("\u{1b}[!p")
            && let Some(after) = rest.strip_prefix(saved_repair.as_str())
        {
            rest = after;
        } else if (out.ends_with('r') || out.ends_with("\u{1b}c"))
            && is_region_or_reset_end(&out)
            && let Some(len) = repair_len(rest, &repair_tail)
        {
            rest = &rest[len..];
        } else {
            let next = rest.chars().next().expect("non-empty");
            out.push(next);
            rest = &rest[next.len_utf8()..];
        }
    }
    out
}

/// Whether `out` stops after an escape or a CSI that has not reached its final byte.
fn ends_inside_a_csi(out: &str) -> bool {
    if out.ends_with('\u{1b}') {
        return true;
    }
    out.rfind("\u{1b}[")
        .is_some_and(|start| out[start + 2..].bytes().all(|b| (0x20..=0x3f).contains(&b)))
}

/// Whether `out` ends in a complete `CSI … r` or `ESC c`.
fn is_region_or_reset_end(out: &str) -> bool {
    if out.ends_with("\u{1b}c") {
        return true;
    }
    let Some(start) = out.rfind("\u{1b}[") else {
        return false;
    };
    let body = &out[start + 2..out.len() - 1];
    body.bytes().all(|b| b.is_ascii_digit() || b == b';')
}

/// The length of a `CSI digits;limit r` at the start of `rest`.
fn repair_len(rest: &str, tail: &str) -> Option<usize> {
    let body = rest.strip_prefix("\u{1b}[")?;
    let digits = body.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0 && body[digits..].starts_with(tail)).then_some(2 + digits + tail.len())
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

fn until(cond: impl FnMut() -> bool) -> bool {
    until_within(BOUND, Duration::from_millis(50), cond)
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
    first_screen_with(bed, harness, &[], &[])
}

/// [`first_screen`] with the operator's own flags and environment.
fn first_screen_with(
    bed: &Bed,
    harness: &str,
    flags: &[String],
    env: &[(String, String)],
) -> Option<(Operator, AgentId)> {
    if !on_path(harness) {
        eprintln!("SKIP: `{harness}` is not on PATH, so its native facade E2E did not run");
        return None;
    }
    let op = Operator::facade(bed, harness, flags, env);
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

        // ---- the status row: marion's one optional line, and neither toggle reaches the node ----
        //
        // A native root has no children here, so the row's counts are all zero; what the clause
        // asserts is that `^] s` takes the bottom line of the *resized* terminal from the node —
        // the node's pty is resized one row shorter — and paints the row there, that the node's
        // screen above it is exactly what the node drew (its own recording, replayed at its own
        // size, is the oracle: a row that split a sequence or scrolled into the node's text shows
        // up as a difference), that a second `^] s` takes the row off and gives the node its full
        // height back, and that both chords stopped at marion: no `i` record.
        let toggle = [marion_tui::keys::PREFIX, marion_tui::keys::STATUS_KEY];
        let reserved = WinSize {
            cols: RESIZED.cols,
            rows: RESIZED.rows - 1,
        };
        op.type_in(&toggle);
        assert!(
            until(|| geometries(&node_cast).last() == Some(&reserved.as_cast())),
            "[{harness}] `^] s` did not give the status row's line back from the node: its pty \
             geometries are {:?}",
            geometries(&node_cast)
        );
        assert!(
            until(|| op.last_viewport_line().contains("marion: 0 children")),
            "[{harness}] `^] s` put no status row on the bottom line, which reads {:?}",
            op.last_viewport_line()
        );
        // Byte for byte, every harness: what reached the operator is the node's own output with
        // only marion's row, its clear and its scroll-region repairs added — none of them inside a
        // node sequence, or the node's bytes would not survive the strip intact.
        let limit = reserved.rows;
        let mut relayed = (String::new(), String::new());
        assert!(
            until(|| {
                relayed = (
                    without_marions_bytes(&cast_text(&op.cast, "o"), limit),
                    cast_text(&node_cast, "o"),
                );
                relayed.0 == relayed.1
            }),
            "[{harness}] the operator's stream is not the node's output plus marion's row: they \
             part at byte {:?}",
            relayed
                .0
                .char_indices()
                .zip(relayed.1.chars())
                .find(|((_, a), b)| a != b)
                .map(|((at, _), _)| (
                    at,
                    tail(&relayed.0[..at]),
                    tail(&relayed.1[..at.min(relayed.1.len())])
                ))
        );
        // And as a screen, where the node repaints all of it on a resize (the alternate screen):
        // the rows above marion's are exactly the node's screen at the node's size. A main-screen
        // TUI's picture after a resize depends on where in its stream the resize landed, which two
        // emulators replaying two recordings cannot reproduce, so for those the bytes above are
        // the proof.
        if on_alternate_screen(&cast_text(&node_cast, "o")) {
            let mut above = (Vec::new(), Vec::new());
            assert!(
                until(|| {
                    above = (op.rows_above_the_last(), node_screen(&node_cast));
                    above.0 == above.1
                }),
                "[{harness}] with the status row shown, the operator's screen above it is not \
                 the node's own screen.\noperator:\n{}\nnode:\n{}",
                above.0.join("\n"),
                above.1.join("\n")
            );
        }
        op.type_in(&toggle);
        assert!(
            until(|| !op.last_viewport_line().contains("marion:")),
            "[{harness}] a second `^] s` did not take the status row off; the bottom line reads {:?}",
            op.last_viewport_line()
        );
        assert!(
            until(|| geometries(&node_cast).last() == Some(&RESIZED.as_cast())),
            "[{harness}] the second `^] s` did not give the node its full height back: {:?}",
            geometries(&node_cast)
        );
        assert!(
            cast_text(&node_cast, "i").is_empty(),
            "[{harness}] a status toggle reached the node as input: {:?}",
            cast_text(&node_cast, "i")
        );

        // ---- detach: the client leaves, the node stays ----
        op.type_in(&[marion_tui::keys::PREFIX, marion_tui::keys::DETACH_KEY]);
        assert!(
            until(|| op.exited()),
            "[{harness}] `marion {harness}` did not exit on ^]d"
        );
        // One line on the restored terminal says how to get back to the node that kept running.
        let hint = format!("marion attach {}", agent.0);
        assert!(
            until(|| cast_text(&op.cast, "o").contains(&hint)),
            "[{harness}] the detach did not tell the operator how to get back ({hint:?}); what the \
             client last wrote:\n{}",
            tail(&cast_text(&op.cast, "o"))
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
        eprintln!(
            "[{harness}] native facade E2E: first screen, keystroke, resize, status row, detach, kill"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Step 10: SIGTERM at the client
// ---------------------------------------------------------------------------------------------

/// `docs/specs/2026-08-13-native-relay-synchronous-signals-design.md`: an owned
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

// ---------------------------------------------------------------------------------------------
// Turn delivery by paste: every lane whose interactive row is `TerminalPaste`
// ---------------------------------------------------------------------------------------------

/// The marker the canned provider answers the native root's pasted turn by. Only the root's own
/// transcript carries it: the steer that starts the root's first turn is the only text it is in.
const PASTE_ROOT_MARKER: &str = "PASTE-ROOT-7f3a";

/// How one `TerminalPaste` lane's operator points their own harness at a loopback provider, the
/// way an operator would: files in a home the harness is told about, and variables. A native node
/// places no provider of its own (§6.4), so this is the only route, exactly as
/// `native_facade_spawn.rs` points `marion claude` at one through `ANTHROPIC_BASE_URL`.
struct CannedOperator {
    tail: Vec<String>,
    env: Vec<(String, String)>,
    /// marion's `spawn` in the spelling this lane's wire declares it under, when the lane's root is
    /// driven through a background delegation too; `None` for a lane whose TUI asks before an MCP
    /// call, so a canned turn cannot reach the bridge without an operator answering it.
    spawn_tool: Option<&'static str>,
}

/// The setup for `harness`, with its files written under `home`; `None` for a lane this suite has
/// no loopback setup for, which the caller turns into a loud failure rather than a skip.
fn canned_operator(
    harness: marion_core::harness::Harness,
    base_url: &str,
    home: &Path,
    project: &Path,
) -> Option<CannedOperator> {
    use marion_core::harness::Harness;
    let s = |v: &str| v.to_string();
    match harness {
        Harness::Codex => {
            // The canned row's own provider table (`codex::config_toml`), with a key variable
            // outside `MARION_*` (the native environment drops that prefix) and the project
            // trusted up front so the TUI's first screen is its composer.
            let codex_home = home.join("codex");
            std::fs::create_dir_all(&codex_home).unwrap();
            let project = std::fs::canonicalize(project).unwrap();
            std::fs::write(
                codex_home.join("config.toml"),
                format!(
                    "model_provider = \"canned\"\napproval_policy = \"never\"\n\
                     sandbox_mode = \"workspace-write\"\n\n[features]\nplugins = false\n\n\
                     [model_providers.canned]\nname = \"canned\"\nbase_url = \"{base_url}\"\n\
                     wire_api = \"responses\"\nenv_key = \"CANNED_PROVIDER_KEY\"\n\n\
                     [projects.\"{}\"]\ntrust_level = \"trusted\"\n",
                    project.display()
                ),
            )
            .unwrap();
            Some(CannedOperator {
                tail: Vec::new(),
                env: vec![
                    (s("CODEX_HOME"), codex_home.display().to_string()),
                    (s("CANNED_PROVIDER_KEY"), s("canned")),
                ],
                spawn_tool: Some("spawn"),
            })
        }
        Harness::OpenCode => {
            // The canned row's isolation (`HOME` and the XDG roots under one directory) and its
            // provider document, written where that relocated root reads it.
            use marion_harness::opencode::{ConfigSpec, ModelRef, config_json, config_path};
            let path = config_path(home);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let doc = config_json(
                &ConfigSpec {
                    model: ModelRef {
                        provider: s("canned"),
                        model: s("canned-1"),
                    },
                    base_url: base_url.to_string(),
                    api_key: Some("canned".into()),
                },
                None,
            );
            std::fs::write(&path, doc.to_string()).unwrap();
            let under = |d: &str| home.join(d).display().to_string();
            Some(CannedOperator {
                tail: Vec::new(),
                env: vec![
                    (s("HOME"), home.display().to_string()),
                    (s("XDG_CONFIG_HOME"), under("config")),
                    (s("XDG_DATA_HOME"), under("data")),
                    (s("XDG_CACHE_HOME"), under("cache")),
                    (s("XDG_STATE_HOME"), under("state")),
                ],
                spawn_tool: None,
            })
        }
        Harness::Copilot => {
            // The canned row's BYOK variables and relocated home, with the project trusted.
            use marion_harness::copilot as c;
            let copilot_home = home.join("copilot");
            std::fs::create_dir_all(&copilot_home).unwrap();
            let project = std::fs::canonicalize(project).unwrap();
            std::fs::write(
                copilot_home.join("config.json"),
                serde_json::json!({ "trusted_folders": [project] }).to_string(),
            )
            .unwrap();
            Some(CannedOperator {
                tail: vec![s("--model"), s("canned-1")],
                env: vec![
                    (s(c::HOME_ENV), copilot_home.display().to_string()),
                    (s(c::PROVIDER_BASE_URL_ENV), base_url.to_string()),
                    (s(c::PROVIDER_TYPE_ENV), s(c::PROVIDER_TYPE)),
                    (s(c::PROVIDER_WIRE_API_ENV), s(c::WIRE_API)),
                    (s(c::PROVIDER_API_KEY_ENV), s("canned")),
                    (s(c::OFFLINE_ENV), s("true")),
                    (s(c::AUTO_UPDATE_ENV), s("false")),
                ],
                spawn_tool: None,
            })
        }
        _ => None,
    }
}

/// The text of every user message in a provider request, on either wire this suite drives:
/// Responses (`input[]` items with `role: user`) and Chat Completions (`messages[]`).
fn user_texts(request: &serde_json::Value) -> Vec<String> {
    use serde_json::Value;
    let items = request
        .pointer("/body/input")
        .or_else(|| request.pointer("/body/messages"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    items
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .flat_map(|m| match m.get("content") {
            Some(Value::String(text)) => vec![text.clone()],
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str).map(str::to_string))
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

/// The first request whose user messages carry `needle`, bounded.
fn user_turn_carrying(
    server: &marion_provider::CannedServer,
    needle: &str,
) -> Option<serde_json::Value> {
    let mut found = None;
    until(|| {
        found = server.requests().ok().and_then(|rs| {
            rs.into_iter()
                .find(|r| user_texts(r).iter().any(|t| t.contains(needle)))
        });
        found.is_some()
    });
    found
}

/// Every `MessageDelivered` the journal holds for `agent`, as `(message id, via)`.
fn deliveries_to(bed: &Bed, agent: &AgentId) -> Vec<(String, String)> {
    let path = bed.project_dir.journal();
    std::fs::read(path)
        .unwrap_or_default()
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(marion_core::journal::decode)
        .filter_map(|r| match r.kind {
            marion_core::journal::RecordKind::MessageDelivered(d) if &d.agent_id == agent => {
                Some((d.message_id, d.via))
            }
            _ => None,
        })
        .collect()
}

fn steer(bed: &Bed, agent: &AgentId, text: &str) -> String {
    let mut client = Client::dial(&bed.paths);
    let id = client.send(marion_core::proto::Call::NodeSteer(
        marion_core::proto::params::NodeSteerParams {
            agent_id: agent.clone(),
            text: text.to_string(),
            caller: None,
        },
    ));
    let (_, outcome) = client.read_to_response(id);
    let marion_core::proto::Outcome::Result(body) = outcome else {
        panic!("node/steer was refused: {outcome:?}")
    };
    let Ok(marion_core::proto::MethodResult::NodeSteer(result)) =
        marion_core::proto::Method::NodeSteer.decode_result(&body)
    else {
        panic!("node/steer answered with the wrong result: {body:?}")
    };
    assert!(result.queued, "node/steer did not queue: {result:?}");
    result.message_id.expect("a queued steer names its message")
}

/// **An operator's steer, and a background child's end, reach a native TUI as user turns** — for
/// every enabled lane whose interactive row is `TerminalPaste` (the row decides; no lane is named).
///
/// The far end is the provider's request log: the message typed into the node's terminal is only
/// delivered if the harness submitted it as a turn, which is a request carrying it as a user
/// message. The journal's `MessageDelivered { via: "pty:paste" }` is marion's half of the same
/// claim, and the node's cast carries the paste under marion's own marker.
///
/// A lane whose loopback setup has a `spawn_tool` also backgrounds a child from that pasted turn:
/// the child is a canned codex node, and when it ends its end is queued for the native root and
/// pasted in, and the root's next request carries it.
///
/// Gated like every clause here: a lane whose harness is not on PATH skips by name.
#[test]
fn a_steer_and_a_background_childs_end_reach_every_native_paste_lane_as_user_turns() {
    use marion_harness::spec::{NodeShape, TurnDelivery, delivery_for};
    let registry = marion_core::production_native_facades();
    let lanes: Vec<(&str, marion_core::harness::Harness)> = registry
        .enabled_native_commands()
        .into_iter()
        .filter_map(|lane| Some((lane, lane.parse().ok()?)))
        .filter(|(_, harness)| {
            matches!(
                delivery_for(
                    marion_harness::adapter::harness_spec(*harness),
                    NodeShape::Interactive
                ),
                TurnDelivery::TerminalPaste { .. }
            )
        })
        .collect();
    assert!(
        !lanes.is_empty(),
        "no enabled native lane takes turns by paste, so this matrix is empty"
    );
    for (lane, harness) in lanes {
        let reqlog = scratch(&format!("native-paste-provider-{lane}"));
        let home = scratch(&format!("native-paste-home-{lane}"));
        // Built before the server so the script can name the lane's spawn spelling.
        let probe =
            canned_operator(harness, "http://127.0.0.1:1/v1", &home, &home).unwrap_or_else(|| {
                panic!(
                    "[{lane}] takes turns by paste but this suite has no loopback setup for it: \
                     add it to `canned_operator`"
                )
            });
        let turns = probe
            .spawn_tool
            .map(|tool| {
                vec![marion_provider::script::ScriptedCall::new(
                    tool,
                    serde_json::json!({
                        "agent_type": "codex-impl",
                        "prompt": "Add the marker file under src/ and report back.",
                        "acceptance_criteria": ["a file exists under src/ containing the marker"],
                        "writable_scope": ["src/**"],
                        "background": true,
                    }),
                )]
            })
            .unwrap_or_default();
        let server = marion_provider::CannedServer::start(marion_provider::Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog: reqlog.join("provider-requests.jsonl"),
            script: marion_provider::Script {
                nodes: vec![marion_provider::script::NodeScript {
                    marker: PASTE_ROOT_MARKER.to_string(),
                    call_prefix: "paste_root".to_string(),
                    turns,
                    final_text: "Noted.".to_string(),
                }],
                ..marion_provider::Script::default()
            },
        })
        .expect("the canned provider binds");
        let base_url = server.base_url();
        let bed = Bed::serving(&format!("native-paste-{lane}"), &base_url, true);
        let setup = canned_operator(harness, &base_url, &home, &bed.project)
            .expect("the same lane as the probe");

        let Some((op, root)) = first_screen_with(&bed, lane, &setup.tail, &setup.env) else {
            continue;
        };
        let mut client = Client::dial(&bed.paths);
        assert!(
            until(|| client
                .tree()
                .iter()
                .any(|n| n.agent_id == root && n.state == marion_core::node::NodeState::Running)),
            "[{lane}] the native root never projected as Running"
        );

        // ---- the operator's steer is typed in and submitted ----
        let text = format!("{PASTE_ROOT_MARKER}: take the next task in the background.");
        let message = steer(&bed, &root, &text);
        let turn = user_turn_carrying(
            &server,
            &format!("marion: message from the operator: {text}"),
        );
        if turn.is_none() && op.exited() {
            unexpected_client_exit(op, &bed, &format!("[{lane}] before the steer arrived"));
        }
        assert!(
            turn.is_some(),
            "[{lane}] no provider request carried the steer as a user message. deliveries: {:?}\n\
             node screen:\n{}\nrequests: {:?}",
            deliveries_to(&bed, &root),
            tail(&cast_text(&bed.node_cast(&root), "o")),
            server
                .requests()
                .map(|rs| rs.iter().map(user_texts).collect::<Vec<_>>())
        );
        assert!(
            deliveries_to(&bed, &root).contains(&(message.clone(), "pty:paste".to_string())),
            "[{lane}] the journal does not say the steer was delivered by paste: {:?}",
            deliveries_to(&bed, &root)
        );
        let node_cast = bed.node_cast(&root);
        assert!(
            cast_records(&node_cast)
                .iter()
                .any(|(code, data)| code == "m" && data.contains(&message)),
            "[{lane}] the node's cast carries no marion marker naming the pasted message"
        );

        // ---- a background child's end is pasted in as the root's next turn ----
        if setup.spawn_tool.is_some() {
            let mut child = None;
            assert!(
                until(|| {
                    child = bed
                        .replayed_nodes()
                        .into_iter()
                        .find(|n| n.parent_id() == Some(&root) && n.exit.is_some());
                    child.is_some() || op.exited()
                }),
                "[{lane}] no background child of the native root ended: {:?}",
                bed.replayed_nodes()
            );
            let Some(child) = child else {
                unexpected_client_exit(op, &bed, &format!("[{lane}] before its child ended"));
            };
            let turn = user_turn_carrying(&server, "you backgrounded as task_id");
            assert!(
                turn.is_some(),
                "[{lane}] the child {} ended but no provider request carried its end as a user \
                 message. deliveries: {:?}\nnode screen:\n{}",
                child.agent_id.0,
                deliveries_to(&bed, &root),
                tail(&cast_text(&node_cast, "o"))
            );
            assert_eq!(
                deliveries_to(&bed, &root)
                    .iter()
                    .filter(|(_, via)| via == "pty:paste")
                    .count(),
                2,
                "[{lane}] the steer and the child's end, each pasted once"
            );
        }

        let node = bed.kill_and_await_exit(&root);
        assert!(node.state.is_exited(), "[{lane}] {:?}", node.state);
        let _ = op.finish();
        eprintln!(
            "[{lane}] paste delivery E2E: steer{} reached the provider as user turns",
            if setup.spawn_tool.is_some() {
                " and a background child's end"
            } else {
                ""
            }
        );
    }
}
