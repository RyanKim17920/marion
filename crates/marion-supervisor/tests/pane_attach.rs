//! **§9's M3 criteria C1 and C2, end to end: a real `claude` and a real `codex` in panes marion
//! owns, driven over the socket by a real `marion attach`.**
//!
//! Every other pty test in this workspace drives `/bin/sh` and registers its host by hand. These
//! start nothing by hand: `marion run <harness> --pane` is a subprocess, the supervisor it wakes is
//! a subprocess, `marion attach` is a third — and the only thing this file constructs is the
//! **operator's terminal**, because there has to be one and a test harness has no tty of its own.
//!
//! Two harnesses and one bed, because the criteria are two readings of one seam and the second is
//! not reachable through the first: C2's subject is codex for a measured reason (§5.3) — it keeps
//! its session on the **main** screen and emits `ESC[3J` on every resize, while Claude Code enters
//! its own alternate screen seconds after boot and an alternate screen has no scrollback to lose.
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
//! * **the alt-screen switch handled** — asserted as an *agreement*: marion's grid must be on
//!   whichever screen the node's own bytes put it on, taking the switch when told and inventing
//!   none when not. Stated that way because the harness moved — see the clause in the test.
//! * **a keystroke through** — asserted as its *consequence* on the operator's own screen: the
//!   trust dialog's cursor moving, the composer appearing, a typed marker echoed back. Never as an
//!   `i` record: `marion attach` negotiates pane-v1, whose keyboard input is evidenced by length
//!   only and leaves no asciicast `i` by design.
//!
//! # What this file does not cover, and where it is covered instead
//!
//! **Covered here**: a real `claude` in a marion-owned pane; a client attaching; bytes rendering;
//! the pre-alt-screen trust dialog rendered on the main screen (§9 names that clause explicitly,
//! and it is this file's render anchor); a keystroke arriving *and changing what the node does*; a
//! resize reaching the one master and the client repainting at the new geometry; marion's own
//! alternate-screen restore not depending on the node's; and detach leaving the node running.
//!
//! **Not covered here, named rather than left to be assumed:**
//!
//! * **the alt-screen switch's *positive* direction, live.** claude 2.1.225 takes no switch at all
//!   — probed 2026-08-08, zero `?1049h` over a boot, a trust dialog, a turn and a resize — so the
//!   agreement asserted here can only catch a spurious switch on this version. The direction that
//!   needs a harness that switches is pinned over the committed 2.1.220 captures, in
//!   `marion-term/tests/replay.rs::the_alt_screen_switch_is_handled_including_the_restore_that_never_arrives`.
//! * **the mouse, live, in either direction.** The *enabling* leg: no harness marion can pane asks
//!   for a mouse today — 2.1.225 and later enable no tracking mode and codex never has (§5.3);
//!   2.1.220 enabled all four, so `attach.rs`'s
//!   `the_nodes_mouse_modes_are_mirrored_onto_the_operators_terminal` pins the mirror against
//!   those sequences rather than against a harness that stopped sending them. The *forwarding*
//!   leg: an SGR-1006 report typed into a pane-v1 client leaves no `i` record and, with tracking
//!   off in the node, no visible consequence, so it is pinned byte-exact over the wire in
//!   `attach.rs`'s `writable_pane_v1_sends_arbitrary_keyboard_bytes_on_node_pane_write` and into a
//!   real child in `handler.rs`'s `a_negotiated_clients_first_opaque_keystroke_reaches_the_child`.
//! * **permission prompt correct** — a pane's permission ask is answered *in the pane*, by the
//!   operator, in the harness's own dialog. That reading is not a shrug: `compile_pane`
//!   deliberately omits `--permission-prompt-tool stdio`, which is the flag that would route the
//!   ask to marion instead (§5.2, §11 item 22), and `marion-harness`'s
//!   `the_pane_argv_is_a_tui_with_the_headless_shape_isolation` asserts the omission. What is left
//!   is the dialog itself, and provoking one needs a paned node **granted a tool it must ask
//!   about**.
//!
//!   This used to read *"today a pane compiles `--tools \"\"`, so claude has nothing to ask
//!   permission for"*. That was false about panes: `claude_code::compile_pane` passes `spec.tools`
//!   through, and `marion run claude-impl --pane` over a git repository compiles
//!   `--tools Read,Write`. The empty axis belonged to the **`claude` agent type**, not to the pane
//!   shape. The true obstacle is one axis further on and is asserted rather than described, in
//!   `marion-harness`'s
//!   `a_paned_node_compiles_its_grant_on_both_axes_and_names_no_permission_prompt_tool`: §3.1's
//!   two axes come from one declaration, so the same names land in `--allowedTools`, which is
//!   pre-approval — a paned node is already allowed to use every tool it has. `MILESTONES.md`'s
//!   M3 entry carries the correction and the two probes the manual session should try.
//! * **the recorded 10-minute manual session** C1 names. A human has to sit at a screen for it;
//!   nothing here or anywhere else can stand in, and C1 is not met without it. `MILESTONES.md`'s
//!   M3 entry carries the runbook.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test pane_attach
//! ```
//!
//! Needs a real `claude` **and** a real `codex` on `PATH` and does **not** skip when either is
//! missing, for the reason `journal_wiring.rs` gives. Every model call is served by the
//! CannedServer: no paid tokens.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::pty::{PtyHost, PtyMaster, StdinPlan, WinSize, spawn_pty};
use marion_testsupport::{fixture_repo, on_path, pinned_version, scratch, sweep};

mod common;
use common::client::{Client, paths_for};

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

/// Typed by the **second** operator, after the first has detached. Distinct from `TYPED_MARK` so
/// that finding it in the node's recording cannot be the first client's keystroke read twice.
const SECOND_MARK: &str = "MARIONAGAIN4e71";

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

/// The trust dialog's accept row **with the cursor on it**, as claude 2.1.261 draws it. The glyph
/// is the whole assertion: the row's text is on screen from the first paint, and only the cursor
/// moving onto it says a keystroke was received.
const TRUST_ACCEPT_SELECTED: &str = "❯ Yes, I trust this folder";

/// How long the screen must stay still after a keystroke before the fixture concludes the harness
/// did not receive it and sends another; also the most `wait_still` spends waiting for a screen
/// that keeps moving. Well above claude's redraw latency through marion, the cast and the emulator
/// (tens of milliseconds), and short enough that a lost key costs one retry rather than a stall.
const KEY_SETTLE: Duration = Duration::from_secs(2);

/// How long the screen must stay unchanged to count as settled after a keystroke moved it. One
/// Ink frame is far shorter; claude 2.1.261's second paint of its trust dialog lands well inside
/// this after the first (measured under 250 ms on a bare pty).
const REDRAW_QUIET: Duration = Duration::from_millis(300);

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
///
/// `agent_type` is a parameter because C1 and C2 name two different harnesses and share one bed.
fn start_paned_run(
    agent_type: &str,
    prompt: &str,
    dir: &Path,
    repo: &Path,
    state: &Path,
    base_url: &str,
) {
    let out = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args([
            "run",
            agent_type,
            // The flag under test. Without it this is M1's launch, byte for byte.
            "--pane",
            "--prompt",
            prompt,
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
        "`marion run {agent_type} --pane` failed: {}\n{err}",
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
//
// `common::Client`, and **deliberately never `attach`ed.** `node/attach` leases the write half,
// and the whole point of this file is that the *real* client holds it. A helper that attached
// here would be competing with the process under test for the keyboard.

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

    /// A write the slave side no longer reads (`EIO`) means `marion attach` has exited, so the
    /// failure carries the last thing the operator saw rather than only the errno.
    fn type_in(&self, bytes: &[u8]) {
        if let Err(error) = self.host.master().write_all(bytes) {
            panic!(
                "writing {bytes:?} to the operator's terminal: {error}. `marion attach` is no \
                 longer reading it; what the operator saw last:\n{}",
                self.screen()
            );
        }
    }

    /// Type `key` until `landed` holds of a screen that has **stopped moving**.
    ///
    /// Two measured facts about claude 2.1.261's trust dialog make this the only safe shape, both
    /// taken on a bare pty with no marion in the path. First, the dialog is painted **twice**: the
    /// first frame is replaced by a fresh copy within a few hundred milliseconds, and a key applied
    /// to the first copy is undone by the second — an arrow that moved `❯` to "Yes" is followed,
    /// unprompted, by a frame with `❯` back on "No, exit", and a `\r` that landed on the first copy
    /// simply never takes effect. Second, the menu is two rows and **wraps**, so re-sending a key
    /// on a timer is wrong: a key that landed and one typed on top of it are two keys.
    ///
    /// So a keystroke is re-sent only after `KEY_SETTLE` of stillness (it was not received), and a
    /// keystroke that did move the screen is judged only after the screen has been still for
    /// `REDRAW_QUIET` — long enough for the second paint to have happened — or `KEY_SETTLE` has
    /// passed without stillness (a spinner). If the still frame does not satisfy `landed`, the
    /// key is typed again: that is the remount putting the cursor back, and the wrap-safe answer
    /// to it is one more key on a settled screen, never two in flight.
    fn type_until(&self, key: &[u8], landed: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + BOUND;
        let mut sent = 0;
        loop {
            let before = self.screen();
            if landed(&before) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "after {sent} keystrokes of {key:?} the screen never settled on what they should \
                 have done, so no keystroke changed what the node drew. What the operator saw:\n\
                 {before}"
            );
            self.type_in(key);
            sent += 1;
            let settle = Instant::now() + KEY_SETTLE;
            while Instant::now() < settle && self.screen() == before {
                std::thread::sleep(Duration::from_millis(50));
            }
            if self.screen() != before {
                self.wait_still();
            }
        }
    }

    /// Return once the screen has not changed for `REDRAW_QUIET`, or after `KEY_SETTLE` if it
    /// never stops (a harness animating a spinner is not one that is still redrawing a keystroke).
    fn wait_still(&self) {
        let give_up = Instant::now() + KEY_SETTLE;
        let mut last = self.screen();
        while Instant::now() < give_up {
            std::thread::sleep(REDRAW_QUIET);
            let now = self.screen();
            if now == last {
                return;
            }
            last = now;
        }
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

    /// **Asked through the host without reaping.** `kill(pid, 0)` succeeds against a *zombie*, so
    /// a liveness probe by signal would report a client that exited cleanly as still running.
    /// Keeping the leader waitable also lets host shutdown perform its process-tree sweep first.
    fn exited(&self) -> bool {
        self.host
            .poll_exited_unreaped()
            .expect("polling the attaching client")
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

/// **An empty script, and that is the design rather than an omission.**
///
/// This pane never submits a turn: the test types into the composer and never presses return
/// after the trust dialog, because what C1 asks is whether a keystroke *arrives*, and submitting
/// would make every assertion after it depend on what a model said. So the provider is here to be
/// the endpoint a canned credential can point at, and nothing else. A script carrying a root turn
/// would advertise a turn that is never taken, and the next reader would take its absence for a
/// failure of this test instead of for its shape.
fn script() -> Script {
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
/// * `PtyHost::write_opaque_input_admitted`: skip the `write_all` to the master. Every keystroke
///   is admitted and evidenced and the dialog's cursor never moves, so `type_until` runs out its
///   bound naming the key that changed nothing.
/// * `marion_tui::guard::leave_bytes`: drop the `?1049l`. The operator is left on the alternate
///   screen by a node that never sent a `?1049l` of its own, which is the whole absent-restore
///   case.
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
    start_paned_run(
        "claude",
        PROMPT_MARK,
        &root_dir,
        &repo,
        &state,
        &server.base_url(),
    );
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
    // **Pre-alt-screen, and not merely "before the composer".** The clause says *"on the main
    // screen"*, and the assertion above would be satisfied by a claude that drew its dialog after
    // switching. So the node's recorded output up to this point must contain no `?1049h` at all —
    // which is the same fact stated where a regression could actually change it.
    assert!(
        !cast_text(&node_cast, "o").contains("\u{1b}[?1049h"),
        "claude had already entered its alternate screen by the time the trust dialog was on the \
         operator's screen, so this run cannot say whether marion renders pre-alt-screen output. \
         §5.3 measured the switch at byte 1900 on a run that shows this dialog"
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
    // **The oracle is the screen, never the recording.** `marion attach` negotiates pane-v1, and
    // pane-v1 keyboard input is evidenced by length only — `PtyHost::write_opaque_input_admitted`
    // writes no asciicast `i` record, by design, so that raw keystrokes are never persisted. This
    // step used to wait for an `i` record and could only ever time out; the delivery leg it meant
    // to cover is `handler.rs`'s `a_negotiated_clients_first_opaque_keystroke_reaches_the_child`,
    // over the real socket into a real child. What a live claude adds is the *consequence*: a key
    // that changes what the node draws has provably crossed the master.
    //
    // **Two facts about claude 2.1.261 shape how the dialog is driven, both measured on a bare pty
    // with no marion in the path (see `Operator::type_until`).** First, the dialog is painted
    // twice, and a key applied to the first copy is undone by the second — which is what happened
    // to the `\r` this step used to type the instant the dialog appeared. Byte by byte, that `\r`
    // was read by `marion attach`, sent as `node/pane-write`, admitted, and written to the node's
    // master with `Ok`; the node then drew nothing new, because the copy of the dialog it had been
    // typed into was about to be replaced. Second, the cursor now defaults to **"No, exit"**
    // (2.1.226 defaulted to accepting), so a bare Enter ends the node instead of accepting the
    // folder.
    //
    // So the first key is arrow-down, and it is its own probe: it submits nothing and its
    // consequence is visible (the `❯` moves to "Yes"). Enter goes through the same probe, its
    // consequence being the composer with the prompt `compile_pane` seeded into argv drawn in it.
    op.type_until(b"\x1b[B", |screen| screen.contains(TRUST_ACCEPT_SELECTED));
    assert!(
        op.screen().contains(TRUST_ACCEPT_SELECTED),
        "the cursor left the accept row before Enter could be typed:\n{}",
        op.screen()
    );
    op.type_until(b"\r", |screen| screen.contains(PROMPT_MARK));

    // The composer echoes what is typed into it, so the marker coming back through claude's own
    // rendering is the keystroke arriving — by the same reasoning `SECOND_MARK` gives below.
    op.type_in(TYPED_MARK.as_bytes());
    assert!(
        until(|| op.screen().contains(TYPED_MARK)),
        "the typed marker never came back through the node's composer. What the operator saw:\n{}",
        op.screen()
    );

    // ---- the alt-screen switch is handled ----
    //
    // **Stated as an agreement rather than as a switch, because which one the harness takes has
    // moved.** §5.3 measured 2.1.220 entering `?1049h` at byte 67 (trusted directory) or 1900
    // (after the trust dialog) and never leaving; probed on 2026-08-08, **2.1.225 takes no switch
    // at all** — a boot, a trust dialog, a submitted turn and a resize at 100x30 emit zero
    // `?1049h`, and the only private modes are `?1004`, `?2004`, `?2026` and `?2031`.
    //
    // So a live assertion that the grid *is* on the alternate screen would pin a harness version
    // rather than marion, and one that it is *not* would break the day claude changes back. What
    // is true of marion on both is that its grid must be on whichever screen the node's own bytes
    // put it on: it must take the switch when told, and must not invent one when not. The positive
    // direction is pinned where it is still live — `marion-term`'s
    // `the_alt_screen_switch_is_handled_including_the_restore_that_never_arrives`, over the
    // committed 2.1.220 captures that do switch.
    let node_bytes = cast_text(&node_cast, "o");
    let switched = match node_bytes.rfind("\u{1b}[?1049h") {
        None => false,
        Some(h) => node_bytes.rfind("\u{1b}[?1049l").is_none_or(|l| l < h),
    };
    assert_eq!(
        replay_node(&node_cast, true).on_alt_screen(),
        switched,
        "marion's grid disagrees with the node about which screen the node is on. The node's own \
         bytes {} it on the alternate screen",
        if switched { "put" } else { "do not put" }
    );

    // ---- the mouse ----
    //
    // **Not asserted live any more, and the reason is the same as the keystroke's.** A mouse report
    // is input: the operator's terminal produces it and `Keys` forwards it untouched, and with
    // pane-v1 that forwarding leaves no `i` record to read back. A click into claude's composer
    // has no visible consequence either — no harness marion can pane enables mouse tracking today
    // (2.1.225 and later enable none; codex never has, §5.3) — so there is nothing on the screen to
    // stand in for the record. Both legs are pinned where they are observable: `attach.rs`'s
    // `writable_pane_v1_sends_arbitrary_keyboard_bytes_on_node_pane_write` proves an SGR report
    // crosses the wire byte-exact, and `handler.rs`'s
    // `a_negotiated_clients_first_opaque_keystroke_reaches_the_child` proves admitted bytes reach
    // the child. The enabling leg, `attach::View::mirror_modes`, is unit-tested against the
    // committed 2.1.220 captures that still carry `?1000h`–`?1006h`.

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

    // **And marion never erased the operator's own scrollback doing it.** `CSI 3J` is
    // erase-saved: `marion_term`'s `Suppressor` swallows the node's, and this is the other side —
    // marion must not emit one of its own. The check is here rather than only in
    // `marion_tui::backend` because the resize above is exactly what provokes a full clear, so a
    // backend that cleared with `3J` would have just thrown away everything above this attach in
    // the operator's terminal.
    assert!(
        !op.seen().contains("\u{1b}[3J"),
        "marion emitted erase-saved at the operator's terminal, which discards the scrollback of \
         whatever they were doing before they attached"
    );

    // ---- detach, and the node is left exactly as it was (§7.3.1) ----
    op.type_in(&[marion_tui::keys::PREFIX, marion_tui::keys::DETACH_KEY]);
    assert!(until(|| op.exited()), "`marion attach` did not exit on ^]d");

    // **The other half of "the alt-screen switch is handled": the restore leg is never exercised,
    // and marion must not wait for it.**
    //
    // Measured (§5.3, and again on this run): claude emits `?1049l` only on a clean exit. This node
    // does not have one — the operator walks away and the supervisor kills it on its bound — so the
    // node's own restore never arrives, and a client that mirrored the node's alt-screen state
    // instead of owning its own would hand the operator's shell back on the alternate screen, with
    // their scrollback apparently gone. `guard::leave_bytes` is unconditional for exactly this
    // reason and this is the case that proves it rather than restating it.
    assert!(
        !cast_text(&node_cast, "o").contains("\u{1b}[?1049l"),
        "the node left its alternate screen on its own, so this run is not the absent-restore case \
         C1 is about and the assertion below no longer proves anything. §5.3 measured `?1049l` only \
         on a clean exit; if that has changed, the measurement moved"
    );
    assert!(
        op.seen().contains("\u{1b}[?1049l"),
        "`marion attach` exited leaving the operator's terminal on the alternate screen. The node \
         never sent a `?1049l`, which is the ordinary case, so marion's own restore is the only \
         thing that can put the shell back. Last bytes marion wrote:\n{}",
        tail(&op.seen())
    );
    // And the mirror it opened is closed too, or the operator's shell keeps emitting mouse reports
    // as text for ever. `leave_bytes` sends all four unconditionally for the same reason.
    assert!(
        op.seen().contains("\u{1b}[?1006l") && op.seen().contains("\u{1b}[?1000l"),
        "marion turned the operator's mouse tracking on and left it on:\n{}",
        tail(&op.seen())
    );
    // **The node is proved alive by using it, not by failing to observe its death.**
    //
    // The first shape of this assertion read the tree once and checked the node was not exited,
    // and it was vacuous: a supervisor that ended the node on detach takes a moment to journal the
    // exit, so the read raced the teardown and passed. Asserting an *absence* over a fixed wait
    // would only have made the race longer.
    //
    // So a second operator attaches to the same node and types. A marker that lands in the node's
    // pty after the first client is gone is positive evidence of three things at once: the node
    // still exists, its pty is still marion's, and the write half the first client held was
    // released — which is §7.3.1's actual content, and the half a crashed client makes urgent.
    let again = Operator::attach(&root_dir, &repo, &state, &agent);
    // **Wait for its first paint before typing into it.** `marion_tui::guard::Screen::enter` puts
    // the terminal into raw mode with `TCSAFLUSH`, which *discards* whatever is already pending on
    // the input queue — so a keystroke sent between spawning the client and its setup is dropped on
    // the floor, and the assertion below would fail for a reason that has nothing to do with the
    // node. This is also the re-attach's own evidence: a second client is rendering the same pane.
    assert!(
        until(|| again.screen().contains(PROMPT_MARK)),
        "the second client attached but never rendered the node:\n{}",
        again.screen()
    );
    again.type_in(SECOND_MARK.as_bytes());
    assert!(
        until(|| again.screen().contains(SECOND_MARK)),
        "after the first client detached, a second one could not drive the node. What is asserted \
         is the *echo*, not the write: a `node/pty-write` lands an `i` record in the recording \
         whether or not anything is alive on the other end of the master, so a marker that comes \
         back **through the harness's own composer** is the only form of this assertion that a \
         dead node fails. What the second operator saw:\n{}",
        again.screen()
    );
    // And the supervisor agrees, on a connection that never attached.
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

    drop(again);
    drop(op);
}

// ---------------------------------------------------------------------------------------------
// C2
// ---------------------------------------------------------------------------------------------

/// Seeded into codex's composer — and **submitted** by codex 0.147.0 the moment the TUI opens,
/// which is where the two panes differ (`marion_harness::codex::compile_tui`).
const CODEX_MARK: &str = "MARIONCODEXPANE5d19";

/// The geometry C2's operator resizes to. Distinct from [`RESIZED`] only so a failure message
/// cannot be misread as C1's.
const CODEX_RESIZED: WinSize = WinSize {
    cols: 108,
    rows: 32,
};

/// How many `/status` panels the operator may ask for before the pane is declared to have produced
/// no scrollback. Three fill a 30-row screen; the rest is headroom for a panel that renders
/// shorter than measured, and the loop stops as soon as [`MIN_HISTORY`] rows exist.
const STATUS_SENDS: usize = 12;

/// How much scrollback this test insists on **before** it resizes anything.
///
/// Not `> 0`, and the difference is the test's margin. The first shape of this asserted merely that
/// some history existed, and the run it passed on had **5** rows against 3 retained without the
/// interception — a two-row verdict, which is close enough to noise that a codex whose panel
/// rendered one line shorter could have inverted it. 20 rows is more than one `/status` panel, so
/// the differential below is a panel wide rather than a rounding error.
const MIN_HISTORY: usize = 20;

/// How long each `/status` is given to render before another is typed. Below codex's own redraw
/// cadence the second submission lands in a composer the first has not left.
const STATUS_SETTLE: Duration = Duration::from_millis(1200);

/// **§9's M3 criterion C2, end to end: a real `codex` TUI in a pane marion owns, and the
/// scrollback it accumulated is still there after a resize.**
///
/// # Why this is not `marion-term/tests/replay.rs::scrollback_survives_codex_resize` again
///
/// That test is the *mechanism*, and it is a good one: it replays a committed asciicast through
/// `marion_term::Term` and pins 16 retained rows against 1. But there is no process in it, no pty,
/// no pane and no resize — the "resize" is an `r` record in a file. C2's sentence is *"a real
/// `codex` TUI **runs in a pane**"*, and until `CodexAdapter::pane_surfaces` existed the launch
/// itself was refused, so the criterion was open however green the replay was.
///
/// Here codex is a real process on a real pty that `marion run codex --pane` opened, a real
/// `marion attach` is rendering it, and the resize is `TIOCSWINSZ` on the **operator's** terminal
/// travelling over the socket to the one master — which is what makes codex emit the `ESC[3J` at
/// all. Nothing about the sequence is scripted by this test.
///
/// # Where the retention is asserted, and why it is not the operator's screen
///
/// The grid that holds the scrollback is the **client's** (`attach.rs`'s `View::term`, built with
/// `marion_tui::grid_options()`), and `marion attach` paints only its *viewport* — there is no
/// scroll key, so no screen the operator's pty could record ever shows a history row. Asserting on
/// `Operator::screen()` would therefore assert nothing about scrollback at all.
///
/// So the assertion replays the **node's own `pty.cast`** — every byte the real codex wrote, and
/// every geometry the master was really set to — through `marion_tui::grid_options()`, which is
/// the client's grid configuration and not a copy of it. That reconstruction is exact: the client
/// is fed those bytes and resized to those geometries and nothing else.
///
/// # Why the assertion cannot pass vacuously
///
/// Three separate guards, because "scrollback survived" is easy to assert about a session that had
/// none:
///
/// 1. **There is history before the resize.** The test waits for it and fails by name if codex
///    never produced any, so a run that scrolled nothing cannot reach the interesting part.
/// 2. **There is an `ESC[3J` to suppress, and it lands after the resize.** Asserted on the node's
///    recorded output. Without this the whole test would pass on a harness that never threatens
///    the history.
/// 3. **The same bytes lose that history with suppression off.** The differential is what makes
///    this an assertion about `CSI 3J` interception rather than about codex being quiet.
///
/// # Mutations
///
/// * `marion_tui::grid_options`: `suppress_erase_saved: false`. Guard 3 fails — and so does the
///   absolute assertion above it, so this dies twice.
/// * `marion_term`'s `Suppressor`: pass `ClearMode::Saved` through. Same two.
/// * `CodexAdapter::pane_surfaces` → `None`. `marion run codex --pane` is refused and the run
///   never starts.
/// * `CodexAdapter::compile_pane` → `codex::compile_exec`. `codex exec --json …` is not the TUI:
///   no pane renders and no history accumulates.
/// * `handler::deliver_input`, `Input::NodeResize`: drop the write. No `r` record, and guard 2
///   never sees the `ESC[3J` the resize provokes.
#[test]
fn a_real_codex_tui_keeps_its_scrollback_across_a_resize_in_a_marion_pane() {
    assert!(
        on_path("codex"),
        "M3's C2 is a claim about a real harness in a real pane. Put `codex` ({}) on PATH.",
        pinned_version("codex")
    );

    let dir = scratch("pane-codex");
    let root_dir = dir.to_path_buf();
    let repo = root_dir.join("repo");
    let state = root_dir.join("state");
    fixture_repo(&root_dir);

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: root_dir.join("provider-requests.jsonl"),
        script: Script::default(),
    })
    .expect("the canned provider starts");

    let _leftovers = Leftovers {
        needle: root_dir.display().to_string(),
    };
    start_paned_run(
        "codex",
        CODEX_MARK,
        &root_dir,
        &repo,
        &state,
        &server.base_url(),
    );
    let paths = paths_for(&state, &repo);
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
        "the paned codex root never appeared in the supervisor's tree"
    );
    let agent = agent.expect("a root");
    let node_cast = marion_core::paths::ProjectDir::new(
        &state,
        &marion_supervisor::socket::project_root(&repo),
    )
    .agent(&agent)
    .pty_cast();
    assert!(
        until(|| node_cast.exists()),
        "the codex node has no pty recording at {}, so `launch_terminal` never opened a pty",
        node_cast.display()
    );

    // **A client is really attached and really rendering it**, so the retention below is asserted
    // about a pane an operator could be watching rather than about a pty nobody reached.
    let op = Operator::attach(&root_dir, &repo, &state, &agent);
    assert!(
        until(|| op.screen().contains("Codex")),
        "codex's TUI never reached the operator's screen. What the operator saw:\n{}",
        op.screen()
    );

    // ---- guard 1: there is scrollback to lose, before anything is resized ----
    //
    // **Produced by driving the pane, not by waiting.** A codex session that is merely *open* fills
    // one screen and repaints it in place for ever: measured on 0.147.0, a boot and a settled turn
    // at 100x30 wrote 2.3 MB to the pty and scrolled **zero** rows into history. So the operator
    // types, and what they type is `/status` — measured to print a 14-row panel into the
    // main-screen transcript, three of which push this pane's banner off the top. It is a slash
    // command rather than a turn on purpose: nothing here depends on what a model said.
    //
    // Codex queues a `\r` typed while a turn is in flight rather than submitting it, so the turn
    // the argv prompt started is waited out first — `esc to interrupt` is codex's own word for
    // "working", and its disappearance is the harness saying it is ready for input.
    assert!(
        until(|| !op.screen().contains("esc to interrupt")),
        "codex's opening turn never settled, so nothing typed into the composer would be \
         submitted. What the operator saw:\n{}",
        op.screen()
    );
    let mut sent = 0usize;
    assert!(
        until(|| {
            if replay_node(&node_cast, true).history_size() >= MIN_HISTORY {
                return true;
            }
            if sent < STATUS_SENDS {
                op.type_in(b"/status\r");
                sent += 1;
                std::thread::sleep(STATUS_SETTLE);
            }
            false
        }),
        "the codex pane never scrolled {MIN_HISTORY} rows into history after {sent} `/status` \
         panels, \
         so there is nothing for a `CSI 3J` to erase and this test would pass whatever marion \
         did. Its recording is {} bytes over {:?}. What the operator saw:\n{}",
        std::fs::metadata(&node_cast).map(|m| m.len()).unwrap_or(0),
        geometries(&node_cast),
        op.screen()
    );
    let before = replay_node(&node_cast, true).history_size();

    // ---- the operator drags the corner of their window ----
    let want = CODEX_RESIZED.as_cast();
    op.resize_window(CODEX_RESIZED);
    assert!(
        until(|| cast_records(&node_cast)
            .iter()
            .any(|(c, d)| c == "r" && *d == want)),
        "the codex node's pty was never resized to {want}, so `TIOCSWINSZ` did not reach the one \
         master. Its recorded geometries were {:?}",
        geometries(&node_cast)
    );

    // ---- guard 2: the resize really provoked an erase-saved ----
    //
    // §5.3: codex emits `ESC[r ESC[0m ESC[H ESC[2J ESC[3J ESC[H` on **every** resize. If a future
    // codex stops, this test must say so rather than keep passing — the criterion is about
    // intercepting a sequence, and a harness that no longer sends it has moved the measurement.
    assert!(
        until(|| erase_saved_after_the_resize(&node_cast, &want)),
        "codex emitted no `ESC[3J` after being resized to {want}. §5.3 measured one per resize on \
         0.146.0 and 0.147.0; without it there is nothing for marion's `Suppressor` to intercept, \
         so C2's mechanism is untested by this run rather than proved by it"
    );

    // ---- the criterion: the history is still there, and only because it was intercepted ----
    let kept = replay_node(&node_cast, true);
    assert!(
        kept.history_size() >= before,
        "the codex pane lost scrollback across the resize: {} rows before, {} after. `CSI 3J` is \
         erase-saved, and marion's whole answer to it is `marion_tui::grid_options`'s \
         `suppress_erase_saved`",
        before,
        kept.history_size()
    );
    assert!(
        kept.stats().suppressed_erase_saved > 0,
        "the grid marion attaches with honoured the node's erase-saved instead of suppressing it"
    );

    // The differential, over the very same bytes: without the interception those pre-resize rows
    // are gone. This is what stops the assertion above from being a statement about codex having
    // been quiet.
    let lost = replay_node(&node_cast, false);
    assert!(
        lost.history_size() < before,
        "with `CSI 3J` honoured the same recording still holds {} of the {} rows it had before \
         the resize, so the assertion above is not measuring the interception. Suppressed: {}",
        lost.history_size(),
        before,
        kept.history_size()
    );

    drop(op);
}

/// Replay a node's `pty.cast` through the grid a marion client attaches with.
///
/// `suppress` is `marion_tui::grid_options()`'s own value when true, and its negation when false —
/// read off the real thing rather than restated, so a client that stopped intercepting cannot
/// leave this helper still describing the old behaviour.
fn replay_node(cast: &Path, suppress: bool) -> marion_term::Term {
    let options = marion_term::Options {
        suppress_erase_saved: suppress && marion_tui::grid_options().suppress_erase_saved,
        ..marion_tui::grid_options()
    };
    // The pane is born at `root::PANE_SIZE`; every later geometry arrives as an `r` record below.
    let mut term = marion_term::Term::with_options(marion_term::Size::new(80, 24), options);
    for (code, data) in cast_records(cast) {
        match code.as_str() {
            "o" => term.advance(data.as_bytes()),
            "r" => resize_from(&mut term, &data),
            _ => {}
        }
    }
    term
}

/// Whether the node wrote an `ESC[3J` **after** it was resized to `geometry`.
///
/// Ordered rather than counted: an erase-saved from somewhere earlier in the session would satisfy
/// a bare `contains`, and the sequence C2 is about is the one the resize provokes.
fn erase_saved_after_the_resize(cast: &Path, geometry: &str) -> bool {
    let records = cast_records(cast);
    let Some(at) = records.iter().position(|(c, d)| c == "r" && d == geometry) else {
        return false;
    };
    records[at..]
        .iter()
        .any(|(c, d)| c == "o" && d.contains("\u{1b}[3J"))
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
