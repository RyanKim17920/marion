//! **§9's M3 criterion C1, end to end: a real `claude` in a pane marion owns, driven over the
//! socket by a real `marion attach`.**
//!
//! Every other pty test in this workspace drives `/bin/sh` and registers its host by hand. This one
//! starts nothing by hand: `marion run claude --pane` is a subprocess, the supervisor it wakes is a
//! subprocess, `marion attach` is a third — and the only thing this file constructs is the
//! **operator's terminal**, because there has to be one and a test harness has no tty of its own.
//!
//! # What each clause of C1 is made to mean here
//!
//! * **bytes rendering** — asserted on the operator's own pty master, not on the node's recording.
//!   The node's `pty.cast` proves claude wrote something; only the operator's master proves marion
//!   emulated it and painted it back out. Those are different failures and one of them is
//!   `marion attach` rendering a blank screen for ever.
//! * **a keystroke arriving** — asserted at the far end, in the node's `pty.cast` `i` record. A
//!   `node/pty-write` that the supervisor accepted and never wrote to the master would satisfy any
//!   assertion made on the client side.
//! * **a resize taking effect** — asserted as an `r` record carrying the operator's geometry. Not
//!   the client's local grid: §5.3 gives the node one writer and the geometry is part of what that
//!   means, so the thing being tested is that `TIOCSWINSZ` reached the one master.
//! * **detach leaves the node running** — §7.3.1. Asserted on the supervisor's own tree over a
//!   *separate* connection, after the attaching process has exited.
//!
//! # What this file does not cover, and where it is covered instead
//!
//! **Covered here**: a real `claude` in a marion-owned pane; a client attaching; bytes rendering;
//! the pre-alt-screen trust dialog rendered on the main screen (§9 names that clause explicitly,
//! and it is this file's render anchor); a keystroke arriving *and changing what the node does*; a
//! resize reaching the one master and the client repainting at the new geometry; and detach
//! leaving the node running.
//!
//! **Not covered here, named rather than left to be assumed:**
//!
//! * **mouse through** — `marion_tui::mouse` decodes the reports and `attach.rs` forwards
//!   keystrokes, but nothing in this file presses a button. It is I5's recorded manual session.
//! * **permission prompt correct** — a pane's permission ask is answered *in the pane*, by the
//!   operator, in the harness's own dialog; marion is not in that loop at all on this surface, so
//!   there is nothing here for a test to assert about marion. I5.
//! * **§9's C2 end to end.** C2 reads *"a real `codex` TUI runs in a pane with scrollback retained
//!   across at least one resize"*, and the subject is codex for a measured reason: it writes to the
//!   **main** screen and emits `CSI 3J`. Claude Code enters its own alternate screen seconds after
//!   boot, and an alternate screen has no scrollback to retain — measured on this very run, which
//!   ends with `history_size == 0` and **zero** erase-saved sequences to suppress. So C2 is not
//!   reachable through the one harness that has a pane shape, and it stays met on the client half
//!   alone (`marion-term/tests/replay.rs::scrollback_survives_codex_resize`, over a committed
//!   capture) until `CodexAdapter::pane_surfaces` exists. Asserting it here against claude would
//!   have been an assertion that passed because there was nothing to lose.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test pane_attach
//! ```
//!
//! Needs a real `claude` on `PATH` and does **not** skip when it is missing, for the reason
//! `journal_wiring.rs` gives. Every model call is served by the CannedServer: no paid tokens.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_proto::{Call, Frame, Method, MethodResult, Outcome, Request, RequestId};
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::pty::{PtyHost, PtyMaster, StdinPlan, WinSize, spawn_pty};
use marion_supervisor::socket::{SocketPaths, socket_paths};
use marion_testsupport::{fixture_repo, on_path, pinned_version, scratch, sweep};

unsafe extern "C" {
    fn getuid() -> u32;
}

/// How long anything here may take before it is a failure. Never a verdict: every assertion below
/// is over a byte, a record or a node's state, and no failure may be repaired by widening this.
const BOUND: Duration = Duration::from_secs(180);

/// The root's wall-clock bound. On a pane `--timeout` *is* the wall clock (there is no `Blocked`
/// state to budget on a node with no typed control plane), so this is how long the node lives if
/// nothing kills it — long enough for an attach, a keystroke, a resize and a detach, and short
/// enough that a failed run cleans itself up.
const ROOT_SECS: &str = "150";

/// Seeded into claude's composer by `compile_pane`, and therefore the string that must appear on
/// the operator's screen if marion is rendering the pane at all. Deliberately one unbroken token:
/// a TUI may wrap around spaces, and an assertion that a *phrase* survived word-wrap is an
/// assertion about the harness's layout engine rather than about marion.
const PROMPT_MARK: &str = "MARIONPANEPROMPT7f3c";

/// Typed into the pane by the test. **No carriage return**: this asserts a keystroke *arrived*, and
/// submitting a turn would make the assertion depend on what the canned provider said next.
const TYPED_MARK: &str = "MARIONTYPED9a2b";

/// The operator's terminal, in the geometry `PANE_SIZE` is not. Different from the launcher's 80x24
/// on purpose — the first thing `marion attach` does is forward its own size, so a test that used
/// 80x24 could not tell a resize that worked from one that never happened.
const OPERATOR_SIZE: WinSize = WinSize {
    cols: 100,
    rows: 30,
};

/// The size the test resizes the operator's terminal to, mid-attach.
const RESIZED: WinSize = WinSize {
    cols: 112,
    rows: 34,
};

// ---------------------------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------------------------

/// Everything a paned run leaves behind, undone whether the test passes or not.
///
/// A guard rather than trailing statements, for `marion_testsupport::Scratch`'s reason: a failing
/// assertion unwinds straight past cleanup, so the runs that leak are exactly the runs that failed.
/// **The node is not this guard's child** — `marion run --pane` returns the moment the node exists
/// and the supervisor owns it from then on, which is the property under test — so the sweep is by
/// scratch-path needle rather than by pid.
struct Leftovers {
    needle: String,
}

impl Drop for Leftovers {
    fn drop(&mut self) {
        // Waited on, not merely signalled: the `Scratch` guard removes this tree the moment this
        // returns, and a SIGKILLed process can still land the write it was already inside.
        sweep(&self.needle);
    }
}

/// Start the paned root and **return when it exists**, which is `marion run --pane`'s whole
/// contract: a node an operator is about to attach to is not a node this call should hold.
fn start_paned_run(dir: &Path, repo: &Path, state: &Path, base_url: &str) {
    let out = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args([
            "run",
            "claude",
            // The flag under test. Without it this is M1's launch, byte for byte.
            "--pane",
            "--prompt",
            PROMPT_MARK,
            "--repo",
            &repo.to_string_lossy(),
            "--state-dir",
            &state.to_string_lossy(),
            "--base-url",
            base_url,
            "--canned",
            "--timeout",
            ROOT_SECS,
        ])
        .current_dir(dir)
        .output()
        .expect("marion run starts");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "`marion run --pane` failed: {}\n{err}",
        out.status
    );
    // The one thing this call is for, besides the node: telling the operator how to reach it.
    assert!(
        err.contains("marion attach "),
        "a paned run must name the attach that reaches it, or the node it just started is \
         unreachable by anything but a `tree/subscribe`:\n{err}"
    );
}

// ---------------------------------------------------------------------------------------------
// A client of the supervisor: used only to find the node and to ask whether it is still running
// ---------------------------------------------------------------------------------------------

/// **Deliberately never attaches.** `node/attach` leases the write half, and the whole point of
/// this file is that the *real* client holds it. A helper that attached here would be competing
/// with the process under test for the keyboard.
struct Client {
    sock: UnixStream,
    lines: BufReader<UnixStream>,
    next_id: i64,
}

impl Client {
    fn dial(paths: &SocketPaths) -> Client {
        let sock = UnixStream::connect(paths.socket()).expect("the supervisor is listening");
        sock.set_read_timeout(Some(BOUND)).unwrap();
        let lines = BufReader::new(sock.try_clone().unwrap());
        Client {
            sock,
            lines,
            next_id: 1,
        }
    }

    fn tree(&mut self) -> Vec<marion_proto::NodeSummary> {
        let id = self.next_id;
        self.next_id += 1;
        let f = Frame::Request(Request::new(
            RequestId::Number(id),
            Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
        ));
        self.sock.write_all(f.to_line().as_bytes()).unwrap();
        self.sock.flush().unwrap();
        loop {
            let mut line = String::new();
            let n = self.lines.read_line(&mut line).expect("a frame arrives");
            assert!(n > 0, "the supervisor closed the connection");
            match Frame::from_line(&line).expect("well-formed frames") {
                Frame::Notification(_) => continue,
                Frame::Response(r) => {
                    let Outcome::Result(body) = r.outcome else {
                        panic!("tree/subscribe was refused")
                    };
                    let MethodResult::TreeSubscribe(s) =
                        Method::TreeSubscribe.decode_result(&body).unwrap()
                    else {
                        panic!("wrong result")
                    };
                    return s.nodes;
                }
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The operator's terminal
// ---------------------------------------------------------------------------------------------

/// `marion attach`, running on a pty this test owns — which is the only way to drive the real
/// client: it refuses to start without a terminal, and rightly so.
///
/// The recording is a second `PtyHost`, so what the operator "saw" is a file this test can read
/// back the same way `pty.cast` is read back for the node. Two hosts, two casts, one direction of
/// travel between them.
struct Operator {
    host: PtyHost,
    cast: PathBuf,
}

impl Operator {
    fn attach(dir: &Path, repo: &Path, state: &Path, agent: &AgentId) -> Operator {
        let cast = dir.join("operator.cast");
        let master = PtyMaster::open(OPERATOR_SIZE).expect("the operator's pty");
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
        cmd.args([
            "attach",
            &agent.0,
            "--repo",
            &repo.to_string_lossy(),
            "--state-dir",
            &state.to_string_lossy(),
        ])
        .current_dir(dir)
        .env("TERM", "xterm-256color");

        // The witness is the pane shape's, exactly as `root::launch_terminal` obtains it: a client
        // that is a full-screen TUI is a node with a display plane, whatever else it is.
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
        .expect("marion attach starts on the operator's pty");
        host.adopt(child);
        Operator { host, cast }
    }

    /// **What the operator can read**, not what arrived on the wire.
    ///
    /// The distinction is the whole point of asserting here. A TUI addresses every word by cursor
    /// position — `trust\x1b[15;20Hthis\x1b[15;25Hfolder` — so a substring search over the byte
    /// stream cannot find a sentence that is plainly on the screen, and a search that *did* match
    /// would be matching a byte marion forwarded rather than a cell marion painted. So the bytes
    /// are put back through a terminal emulator and the grid is read out, which is the operation a
    /// human eye performs.
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

    /// The raw byte stream, for the one assertion that is genuinely about bytes: marion attach's
    /// own read-only banner, which it writes before any emulator exists.
    fn seen(&self) -> String {
        cast_text(&self.cast, "o")
    }

    fn type_in(&self, bytes: &[u8]) {
        self.host
            .master()
            .write_all(bytes)
            .expect("writing to the operator's terminal");
    }

    /// The operator drags the corner of their window. `TIOCSWINSZ` on the operator's master is what
    /// delivers `SIGWINCH` to `marion attach`, which is the signal its resize path is written
    /// against — a test that called some marion function directly would be testing the function it
    /// is trying to prove is reached.
    fn resize_window(&self, size: WinSize) {
        self.host
            .master()
            .set_size(size)
            .expect("resizing the operator's terminal");
    }

    /// **Asked through the host, which reaps.** `kill(pid, 0)` succeeds against a *zombie*, and
    /// nothing else in this test ever waits on `marion attach` — so a liveness probe by signal
    /// would report a client that exited cleanly as still running, for ever.
    fn exited(&self) -> bool {
        self.host
            .try_wait()
            .expect("waiting on the attaching client")
            .is_some()
    }
}

// ---------------------------------------------------------------------------------------------
// Reading a cast
// ---------------------------------------------------------------------------------------------

/// Every record of one code in an asciicast, concatenated. `o` is what the terminal showed, `i` is
/// what was typed into it, `r` is each geometry it was resized to.
fn cast_text(path: &Path, code: &str) -> String {
    cast_records(path)
        .into_iter()
        .filter(|(c, _)| c == code)
        .map(|(_, d)| d)
        .collect()
}

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

fn paths_for(state: &Path, repo: &Path) -> SocketPaths {
    socket_paths(
        state,
        &marion_supervisor::socket::project_root(repo),
        // SAFETY: no arguments, no pointers.
        unsafe { getuid() },
    )
}

/// The one script: the pane never submits a turn in this test, so the provider is here to be the
/// endpoint a canned credential points at rather than to answer anything.
fn script() -> Script {
    // `root: None` — this pane never submits a turn, so the provider is here to be the endpoint a
    // canned credential points at rather than to answer anything. A script with a root turn in it
    // would suggest a turn is expected, and a reader would then take the absence of one as a
    // failure of this test rather than as its design.
    Script::default()
}

// ---------------------------------------------------------------------------------------------
// C1
// ---------------------------------------------------------------------------------------------

/// **The criterion, in one run.**
///
/// # Mutations
///
/// * `root::launch_terminal`: skip `owner.opened(...)`. The attach is refused for having no display
///   plane and the operator's screen carries marion's own refusal instead of claude's TUI.
/// * `handler::deliver_input`, `Input::NodePtyWrite` arm: drop the write. The `i` record never
///   appears.
/// * `handler::deliver_input`, `Input::NodeResize` arm: drop the resize. No `r` record carries the
///   operator's geometry.
/// * `attach::Session::start_keyboard`, `Action::Detach`: fall through to `Forward` instead of
///   setting `leaving`. The client never exits.
/// * `root::launch_terminal`: kill the node when the pane is forgotten. The node is gone after the
///   detach and §7.3.1 is broken.
#[test]
fn a_real_claude_runs_in_a_pane_a_client_attaches_types_resizes_and_detaches_without_ending_it() {
    assert!(
        on_path("claude"),
        "M3's C1 is a claim about a real harness in a real pane. Put `claude` ({}) on PATH.",
        pinned_version("claude")
    );

    let dir = scratch("pane-attach");
    let root_dir = dir.to_path_buf();
    let repo = root_dir.join("repo");
    let state = root_dir.join("state");
    fixture_repo(&root_dir);

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: root_dir.join("provider-requests.jsonl"),
        script: script(),
    })
    .expect("the canned provider starts");

    let _leftovers = Leftovers {
        needle: root_dir.display().to_string(),
    };
    start_paned_run(&root_dir, &repo, &state, &server.base_url());
    let paths = paths_for(&state, &repo);

    // The supervisor and the node, found the way any client would find them.
    assert!(
        until(|| paths.socket().exists()),
        "no supervisor ever came up for this project"
    );
    let mut client = Client::dial(&paths);
    let mut agent = None;
    assert!(
        until(|| {
            agent = client
                .tree()
                .into_iter()
                .find(|n| n.depth == 0)
                .map(|n| n.agent_id);
            agent.is_some()
        }),
        "the paned root never appeared in the supervisor's tree"
    );
    let agent = agent.expect("a root");

    // Its recording, which is where the far end of everything below is asserted.
    let node_cast = marion_core::paths::ProjectDir::new(
        &state,
        &marion_supervisor::socket::project_root(&repo),
    )
    .agent(&agent)
    .pty_cast();
    assert!(
        until(|| node_cast.exists()),
        "the node has no pty recording at {}, so `launch_terminal` never opened a pty for it",
        node_cast.display()
    );

    // ---- the client attaches, and bytes render on the operator's own screen ----
    //
    // The anchor is claude's **trust dialog**, which is what a fresh checkout gets, and it is a
    // better anchor than anything painted later: §9's C1 names *"pre-alt-screen trust dialog
    // rendered on the main screen"* as a clause of its own, and this is that clause. It is drawn
    // by claude before it enters its own alternate screen, so a pane that only forwarded
    // alt-screen frames would show nothing here at all.
    let op = Operator::attach(&root_dir, &repo, &state, &agent);
    assert!(
        until(|| op.screen().contains("trust this folder")),
        "claude's trust dialog never reached the operator's screen. Either the pane was not \
         registered (`marion attach` prints its own refusal in that case) or nothing was \
         rendered. What the operator saw:\n{}",
        op.screen()
    );
    // **And this client got the keyboard.** `marion attach` prints its read-only banner when
    // somebody else holds the write half, and a run that watched its own paned node over the
    // socket used to be that somebody — for the node's entire life, from a process with no
    // terminal to type from.
    assert!(
        !op.seen().contains("read-only"),
        "the operator was refused the write half of a node they started. Something else attached \
         first and is holding a keyboard it cannot use:\n{}",
        tail(&op.seen())
    );

    // ---- a keystroke arrives, and changes what the node does ----
    //
    // Asserted twice over, because the two halves fail separately. The `i` record is the far end:
    // a `node/pty-write` the supervisor accepted and never wrote to the master would satisfy
    // anything asserted on the client side. The screen is the *consequence*: claude dismissed its
    // dialog and drew the composer, with the prompt `compile_pane` seeded into argv in it.
    op.type_in(b"\r");
    assert!(
        until(|| cast_text(&node_cast, "i").contains('\r')),
        "the keystroke never reached the node's pty. Its recorded input was {:?}",
        cast_text(&node_cast, "i")
    );
    assert!(
        until(|| op.screen().contains(PROMPT_MARK)),
        "the node never got past its trust dialog, so the keystroke reached the pty and did \
         nothing — or the prompt marion seeded into the composer was never drawn. What the \
         operator saw:\n{}",
        op.screen()
    );

    op.type_in(TYPED_MARK.as_bytes());
    assert!(
        until(|| cast_text(&node_cast, "i").contains(TYPED_MARK)),
        "the typed marker never reached the node's pty. Its recorded input was {:?}",
        cast_text(&node_cast, "i")
    );

    // ---- a resize takes effect on the one master ----
    let want = RESIZED.as_cast();
    op.resize_window(RESIZED);
    assert!(
        until(|| cast_records(&node_cast)
            .iter()
            .any(|(c, d)| c == "r" && *d == want)),
        "the node's pty was never resized to {want}, so `TIOCSWINSZ` did not reach the one \
         master. Its recorded geometries were {:?}",
        geometries(&node_cast)
    );
    // Not a tautology beside the line above: the launcher's own 80x24 must be there too, or the
    // "resize" being asserted is a pane that was born at the operator's size and never moved.
    assert!(
        geometries(&node_cast).len() >= 2,
        "the node only ever had one geometry ({:?}), so nothing was resized",
        geometries(&node_cast)
    );

    // **The pane is still being rendered at the new geometry**, which is the half of a resize the
    // `r` record cannot show. A client that forwarded `TIOCSWINSZ` and then stopped repainting —
    // or repainted into a grid still 100 columns wide — passes every assertion above and shows the
    // operator a frozen or clipped screen.
    assert!(
        until(|| {
            let mut term = marion_term::Term::with_options(
                marion_term::Size::new(RESIZED.cols as usize, RESIZED.rows as usize),
                marion_tui::grid_options(),
            );
            for (code, data) in cast_records(&op.cast) {
                match code.as_str() {
                    "o" => term.advance(data.as_bytes()),
                    "r" => resize_from(&mut term, &data),
                    _ => {}
                }
            }
            // Read at the **new** width: a repaint that never happened leaves the reflowed old
            // frame, which loses the marker at the wrap point rather than redrawing it.
            term.viewport_lines().join("\n").contains(PROMPT_MARK)
                && term.size().cols == RESIZED.cols as usize
        }),
        "after the resize the operator's screen no longer shows the node at the new geometry:\n{}",
        op.screen()
    );

    // ---- detach, and the node is left exactly as it was (§7.3.1) ----
    op.type_in(&[marion_tui::keys::PREFIX, marion_tui::keys::DETACH_KEY]);
    assert!(until(|| op.exited()), "`marion attach` did not exit on ^]d");
    // Asserted over a second connection, after the client is gone: the node is the supervisor's,
    // not the client's, and a detach that ended it would be §7.3.1 broken.
    let mut after = Client::dial(&paths);
    let node = after
        .tree()
        .into_iter()
        .find(|n| n.agent_id == agent)
        .expect("the node is still in the tree");
    assert!(
        !node.state.is_exited(),
        "the node died when its client detached: {:?}. §7.3.1 — a client going away must leave \
         every node exactly as it was",
        node.state
    );

    drop(op);
}

/// Every geometry the node's pty was ever set to, in order.
fn geometries(cast: &Path) -> Vec<String> {
    cast_records(cast)
        .into_iter()
        .filter(|(c, _)| c == "r")
        .map(|(_, d)| d)
        .collect()
}

/// Apply one `r` record's geometry to a replaying grid.
fn resize_from(term: &mut marion_term::Term, record: &str) {
    if let Some((c, r)) = record.split_once('x')
        && let (Ok(c), Ok(r)) = (c.parse(), r.parse())
    {
        term.resize(marion_term::Size::new(c, r));
    }
}

fn tail(s: &str) -> String {
    let n = s.len().saturating_sub(2000);
    s[n..].escape_debug().to_string()
}
