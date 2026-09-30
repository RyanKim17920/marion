//! Tests for terminal-paste delivery.
//!
//! The pure parts (framing, the gate) are tested on values. The injector runs against a real pty
//! with **no child**: the test holds the slave in raw mode, so what a read of it returns is what a
//! harness would have read, and it plays the node (writing `CSI ? 2004 h`) and the operator
//! (writing through a lease) by hand. Every wait is on a causal marker — a gate decision the
//! injector reports, a journal record, bytes on the slave — never on a sleep.

use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_core::journal::{MessageDelivered, MessageDropped, RecordKind};
use marion_harness::spec::{BootDialog, BootSignal, DialogAnswer, TurnDelivery};

use super::*;
use crate::inbox::{Inboxes, Source, render};
use crate::pty::{PtyHost, PtyMaster, WinSize};
use crate::serve::ConnId;

const PASTE: TurnDelivery = TurnDelivery::bracketed_paste(BootSignal::FirstDraw, "test row");
/// A row whose TUI draws a provisional composer first (codex 0.155.1's startup draft).
const TITLED_PASTE: TurnDelivery =
    TurnDelivery::bracketed_paste(BootSignal::WindowTitle, "test row");

fn agent() -> AgentId {
    AgentId("paste-node".into())
}

// ---------------------------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------------------------

#[test]
fn a_message_is_framed_as_one_bracketed_paste() {
    assert_eq!(frame("hi"), "\x1b[200~hi\x1b[201~");
    assert_eq!(
        frame("a\nb\tc"),
        "\x1b[200~a\nb\tc\x1b[201~",
        "newlines and tabs are text"
    );
}

/// **Nothing inside the brackets can end them or type a key.** A child's result is quoted into
/// the parent's paste; an `ESC[201~` in it would close the paste early and the rest would arrive
/// as typing — its first newline a submit. So every control but newline and tab goes, and a CR
/// becomes the newline it means.
#[test]
fn control_characters_cannot_escape_the_paste() {
    let hostile = "ok\x1b[201~\rrm -rf /\r\n\x03\x7f\u{9b}201~done";
    let framed = frame(hostile);
    let inner = framed
        .strip_prefix("\x1b[200~")
        .and_then(|f| f.strip_suffix("\x1b[201~"))
        .expect("one bracket each side");
    assert!(
        !inner.contains('\x1b') && !inner.contains('\r'),
        "{inner:?}"
    );
    assert!(
        !inner
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t'),
        "{inner:?}"
    );
    assert_eq!(inner, "ok[201~\nrm -rf /\n201~done");
}

// ---------------------------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------------------------

fn policy() -> PastePolicy {
    PastePolicy {
        operator_quiet: Duration::from_millis(1500),
        paste_mode_grace: Duration::from_secs(30),
        dialog_bound: Duration::from_secs(600),
        recheck: Duration::from_millis(100),
    }
}

/// A terminal that booted long ago: drawn, and quiet since well before any look.
fn state(bracketed: bool, typed: Option<Instant>, empty: bool) -> crate::pty::InputState {
    crate::pty::InputState {
        bracketed_paste: bracketed,
        last_operator_input: typed,
        composer_empty: empty,
        screen_drawn: true,
        titled: true,
        last_output: Some(Instant::now() - Duration::from_secs(60)),
        boot_dialog: None,
    }
}

const SETTLE: Duration = Duration::from_millis(1500);

/// Before the first paste, in a directory marion may not answer dialogs in.
const BOOT: Option<Boot> = Some(Boot {
    settle: SETTLE,
    mark: BootSignal::FirstDraw,
    answer_dialogs: false,
});

/// [`BOOT`], for a row whose boot mark is its window title.
const WINDOW_TITLE: Option<Boot> = Some(Boot {
    settle: SETTLE,
    mark: BootSignal::WindowTitle,
    answer_dialogs: false,
});

/// The measured claude dialog, answerable, and the marker-free fallback the row holds on.
const TRUST: BootDialog = BootDialog {
    needle: "❯ No, exit Yes, I trust this folder",
    action: "trust {repo} once",
    answer: DialogAnswer::Keys(b"\x1b[B\r"),
    note: "test: claude-like folder trust, default No",
};
const TRUST_HELD: BootDialog = BootDialog {
    needle: "Is this a project you created or one you trust?",
    action: "trust {repo} once",
    answer: DialogAnswer::Hold,
    note: "test: the same dialog, selection unmeasured",
};
const CLAUDE_LIKE: &[BootDialog] = &[TRUST, TRUST_HELD];

fn showing(d: &'static BootDialog) -> crate::pty::InputState {
    crate::pty::InputState {
        boot_dialog: Some(d),
        ..state(true, None, true)
    }
}

/// **A dialog on the screen holds the first paste** — typed into it, the paste would answer it
/// (claude's folder trust quits on CR) — and past the grace the message is refused naming it.
/// Past the first paste the dialog is not asked about: the host stops reading for dialogs then.
#[test]
fn a_boot_dialog_holds_the_first_paste_then_is_refused_by_name() {
    let now = Instant::now();
    let p = policy();
    let mut clock = None;
    assert_eq!(
        gate(&showing(&TRUST_HELD), now, &p, BOOT, &mut clock),
        Gate::Wait(Hold::BootDialog, now + p.recheck)
    );
    // The paste grace is not the dialog's: the hold is on the operator's queue, so it waits for
    // them up to the node's own bound.
    assert!(matches!(
        gate(
            &showing(&TRUST_HELD),
            now + p.paste_mode_grace,
            &p,
            BOOT,
            &mut clock
        ),
        Gate::Wait(Hold::BootDialog, _)
    ));
    assert!(matches!(
        gate(&showing(&TRUST_HELD), now + p.dialog_bound, &p, BOOT, &mut clock),
        Gate::Refuse(reason) if reason.contains(TRUST_HELD.needle) && reason.contains("dialog")
    ));
    let typing = crate::pty::InputState {
        last_operator_input: Some(now),
        ..showing(&TRUST_HELD)
    };
    assert_eq!(
        gate(&typing, now, &p, BOOT, &mut None),
        Gate::Wait(Hold::OperatorTyping, now + p.operator_quiet),
        "an operator answering the dialog is waited for as any typing is"
    );
}

/// **An answerable dialog is answered only where marion may answer it**: with the row's keys in
/// a workspace marion created, held like any other dialog everywhere else.
#[test]
fn an_answerable_dialog_is_answered_only_in_marions_own_workspace() {
    let now = Instant::now();
    let p = policy();
    let marions = Some(Boot {
        settle: SETTLE,
        mark: BootSignal::FirstDraw,
        answer_dialogs: true,
    });
    assert_eq!(
        gate(&showing(&TRUST), now, &p, marions, &mut None),
        Gate::Answer(&TRUST)
    );
    assert_eq!(
        gate(&showing(&TRUST), now, &p, BOOT, &mut None),
        Gate::Wait(Hold::BootDialog, now + p.recheck)
    );
    assert_eq!(
        gate(&showing(&TRUST_HELD), now, &p, marions, &mut None),
        Gate::Wait(Hold::BootDialog, now + p.recheck),
        "a held dialog is never answered"
    );
}

#[test]
fn the_gate_opens_only_on_a_quiet_empty_bracketed_terminal() {
    let now = Instant::now();
    let p = policy();
    let mut off = None;
    assert_eq!(
        gate(&state(true, None, true), now, &p, None, &mut off),
        Gate::Ready
    );
    let long_ago = now - Duration::from_secs(2);
    assert_eq!(
        gate(&state(true, Some(long_ago), true), now, &p, None, &mut off),
        Gate::Ready
    );

    let just_now = now - Duration::from_millis(400);
    assert_eq!(
        gate(&state(true, Some(just_now), true), now, &p, None, &mut off),
        Gate::Wait(Hold::OperatorTyping, just_now + p.operator_quiet),
        "quiet is measured from the operator's last key"
    );
    assert_eq!(
        gate(&state(true, Some(long_ago), false), now, &p, None, &mut off),
        Gate::Wait(Hold::ComposerNotEmpty, now + p.recheck),
        "a half-typed line waits for the operator however long ago it was typed"
    );
}

/// Bracketed paste off holds the message for the grace, counted from the first time it was the
/// only thing in the way, and past the grace refuses it. Turning on within the grace resets it.
#[test]
fn bracketed_paste_off_holds_then_refuses_after_the_grace() {
    let t0 = Instant::now();
    let p = policy();
    let mut off = None;
    let off_state = state(false, None, true);
    assert_eq!(
        gate(&off_state, t0, &p, None, &mut off),
        Gate::Wait(Hold::NoBracketedPaste, t0 + p.recheck)
    );
    let later = t0 + Duration::from_secs(10);
    assert_eq!(
        gate(&off_state, later, &p, None, &mut off),
        Gate::Wait(Hold::NoBracketedPaste, later + p.recheck),
        "the grace runs from t0, not from each look"
    );
    assert!(matches!(
        gate(&off_state, t0 + p.paste_mode_grace, &p, None, &mut off),
        Gate::Refuse(reason) if reason.contains("bracketed paste")
    ));
    assert_eq!(
        off, None,
        "a refusal starts the next message's grace afresh"
    );

    let mut off = None;
    gate(&off_state, t0, &p, None, &mut off);
    assert_eq!(
        gate(&state(true, None, true), t0, &p, None, &mut off),
        Gate::Ready
    );
    assert_eq!(off, None, "on again resets the grace");
}

/// **Before the first paste the terminal must have booted**: drawn text under bracketed paste,
/// then been quiet for the row's settle time. Undrawn holds on the recheck; drawn but recently
/// busy holds until exactly the quiet instant; past boot (`None`) neither matters. Never booting
/// is refused after the grace, by its own reason.
#[test]
fn the_first_paste_waits_for_the_terminal_to_boot() {
    let now = Instant::now();
    let p = policy();
    let mut clock = None;
    let undrawn = crate::pty::InputState {
        screen_drawn: false,
        last_output: Some(now),
        ..state(true, None, true)
    };
    assert_eq!(
        gate(&undrawn, now, &p, BOOT, &mut clock),
        Gate::Wait(Hold::Booting, now + p.recheck)
    );
    let busy_at = now - Duration::from_millis(500);
    let busy = crate::pty::InputState {
        last_output: Some(busy_at),
        ..state(true, None, true)
    };
    assert_eq!(
        gate(&busy, now, &p, BOOT, &mut clock),
        Gate::Wait(Hold::Booting, busy_at + SETTLE),
        "quiet is measured from the node's last output"
    );
    assert_eq!(
        gate(&busy, busy_at + SETTLE, &p, BOOT, &mut clock),
        Gate::Ready
    );
    assert_eq!(clock, None, "ready resets the grace");
    assert_eq!(
        gate(&busy, now, &p, None, &mut clock),
        Gate::Ready,
        "past the first paste a busy terminal is typed into: S31 measured busy input is queued"
    );

    gate(&undrawn, now, &p, BOOT, &mut clock);
    assert!(matches!(
        gate(&undrawn, now + p.paste_mode_grace, &p, BOOT, &mut clock),
        Gate::Refuse(reason) if reason.contains("booting")
    ));
    let off = crate::pty::InputState {
        bracketed_paste: false,
        ..undrawn
    };
    assert_eq!(
        gate(&off, now, &p, BOOT, &mut None),
        Gate::Wait(Hold::NoBracketedPaste, now + p.recheck),
        "bracketed paste is asked about first: without it there is no boot to wait for"
    );
}

/// **A window-title row's boot is counted from the title, not the first draw.** A drawn screen
/// that has been quiet for any length of time is still a provisional composer until the title
/// comes; from the title, the same settle applies; a first-draw row does not care about titles.
/// A dialog still comes first: a titled screen showing one is held for the dialog. A title that
/// never comes — the operator can switch it off — is not a TUI still booting: at the grace a
/// drawn screen is pasted into, with a note; an undrawn one is still refused.
#[test]
fn a_window_title_row_boots_on_its_title_and_not_on_its_first_draw() {
    let now = Instant::now();
    let p = policy();
    let mut clock = None;
    let provisional = crate::pty::InputState {
        titled: false,
        ..state(true, None, true)
    };
    assert_eq!(
        gate(&provisional, now, &p, WINDOW_TITLE, &mut clock),
        Gate::Wait(Hold::Booting, now + p.recheck),
        "drawn and quiet for a minute, but untitled: still booting"
    );
    assert_eq!(
        gate(&provisional, now, &p, BOOT, &mut None),
        Gate::Ready,
        "a first-draw row is booted by the same screen"
    );
    let titled_at = now - Duration::from_millis(500);
    let titled = crate::pty::InputState {
        titled: true,
        last_output: Some(titled_at),
        ..provisional
    };
    assert_eq!(
        gate(&titled, now, &p, WINDOW_TITLE, &mut clock),
        Gate::Wait(Hold::Booting, titled_at + SETTLE),
        "the title starts the quiet, as a first draw does"
    );
    assert_eq!(
        gate(&titled, titled_at + SETTLE, &p, WINDOW_TITLE, &mut clock),
        Gate::Ready
    );
    assert_eq!(clock, None, "ready resets the grace");
    let redrawing = crate::pty::InputState {
        screen_drawn: false,
        ..titled
    };
    assert_eq!(
        gate(&redrawing, titled_at + SETTLE, &p, WINDOW_TITLE, &mut None),
        Gate::Wait(Hold::Booting, titled_at + SETTLE + p.recheck),
        "titled, but the screen is forgotten after marion answered a dialog: wait for the redraw"
    );
    let dialog = crate::pty::InputState {
        titled: true,
        ..showing(&TRUST_HELD)
    };
    assert_eq!(
        gate(&dialog, now, &p, WINDOW_TITLE, &mut None),
        Gate::Wait(Hold::BootDialog, now + p.recheck),
        "a dialog on the screen holds the paste whatever the boot mark says"
    );

    gate(&provisional, now, &p, WINDOW_TITLE, &mut clock);
    assert_eq!(
        gate(
            &provisional,
            now + p.paste_mode_grace / 2,
            &p,
            WINDOW_TITLE,
            &mut clock
        ),
        Gate::Wait(Hold::Booting, now + p.paste_mode_grace / 2 + p.recheck),
        "held until the grace"
    );
    assert!(matches!(
        gate(&provisional, now + p.paste_mode_grace, &p, WINDOW_TITLE, &mut clock),
        Gate::Graced(note) if note.contains("window title") && note.contains("grace")
    ));
    assert_eq!(clock, None, "a graced paste resets the grace");

    gate(&dialog, now, &p, WINDOW_TITLE, &mut clock);
    assert!(
        matches!(
            gate(&dialog, now + p.dialog_bound, &p, WINDOW_TITLE, &mut clock),
            Gate::Refuse(reason) if reason.contains("dialog")
        ),
        "a dialog nobody dismissed is still refused by name, never graced"
    );
    let undrawn = crate::pty::InputState {
        screen_drawn: false,
        ..provisional
    };
    gate(&undrawn, now, &p, WINDOW_TITLE, &mut clock);
    assert!(
        matches!(
            gate(&undrawn, now + p.paste_mode_grace, &p, WINDOW_TITLE, &mut clock),
            Gate::Refuse(reason) if reason.contains("booting") && reason.contains("window title")
        ),
        "nothing drawn is nothing to paste into"
    );
    let undrawn_first_draw = crate::pty::InputState {
        titled: true,
        ..undrawn
    };
    gate(&undrawn_first_draw, now, &p, BOOT, &mut clock);
    assert!(
        matches!(
            gate(&undrawn_first_draw, now + p.paste_mode_grace, &p, BOOT, &mut clock),
            Gate::Refuse(reason) if reason.contains("drew a screen")
        ),
        "a mark nothing can switch off, missing at the grace, is a TUI that never booted"
    );
}

#[test]
fn only_a_terminal_paste_row_gets_an_injector() {
    assert!(PasteParams::of(PASTE).is_some());
    for other in [
        TurnDelivery::TypedTurn {
            mid_turn: marion_harness::spec::MidTurn::Queue,
            note: "",
        },
        TurnDelivery::Continuation { note: "" },
        TurnDelivery::McpChannel { note: "" },
        TurnDelivery::None { note: "" },
    ] {
        assert!(PasteParams::of(other).is_none(), "{other:?}");
    }
    assert_eq!(
        PasteParams::of(TITLED_PASTE).unwrap().boot_mark,
        BootSignal::WindowTitle,
        "the row's boot mark is carried as stated"
    );
    let p = PasteParams::of(PASTE).unwrap();
    assert_eq!(p.boot_mark, BootSignal::FirstDraw);
    assert_eq!(
        (p.submit, p.submit_delay, p.settle),
        (
            &b"\r"[..],
            Duration::from_millis(50),
            Duration::from_millis(1500)
        ),
        "the row's idle signal is the boot settle"
    );
    // A row's own injector carries the dialogs its TUI shows at boot, and only a paste row has one.
    for h in marion_core::harness::Harness::ALL {
        let row = marion_harness::adapter::harness_spec(h);
        let interactive =
            marion_harness::spec::delivery_for(row, marion_harness::spec::NodeShape::Interactive);
        match PasteParams::for_row(row) {
            Some(p) => assert_eq!(
                (Some(p.submit), p.dialogs),
                (
                    PasteParams::of(interactive).map(|q| q.submit),
                    row.boot_dialogs.dialogs
                ),
                "{h}"
            ),
            None => assert!(PasteParams::of(interactive).is_none(), "{h}"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The injector, on a real pty
// ---------------------------------------------------------------------------------------------

struct Bed {
    _dir: marion_testsupport::Scratch,
    cast: std::path::PathBuf,
    host: Arc<PtyHost>,
    /// The cast's clock origin, so a cast timestamp and an `Instant` can be compared.
    origin: Instant,
    slave: std::fs::File,
    inboxes: Arc<Inboxes>,
    records: Arc<Mutex<Vec<RecordKind>>>,
    decisions: Receiver<Gate>,
    injector: Arc<PasteInjector>,
}

impl Bed {
    fn new(tag: &str, policy: PastePolicy) -> Bed {
        Bed::booting(tag, policy, &[], false)
    }

    /// A bed whose row shows `dialogs` at boot, in a workspace marion may answer them in or not.
    fn booting(
        tag: &str,
        policy: PastePolicy,
        dialogs: &'static [BootDialog],
        marions_workspace: bool,
    ) -> Bed {
        Bed::with_row(tag, policy, PASTE, dialogs, marions_workspace)
    }

    /// [`Bed::booting`], for a paste row of the caller's.
    fn with_row(
        tag: &str,
        policy: PastePolicy,
        row: TurnDelivery,
        dialogs: &'static [BootDialog],
        marions_workspace: bool,
    ) -> Bed {
        let dir = marion_testsupport::scratch(tag);
        let cast = dir.join("pty.cast");
        let size = WinSize::new(80, 24);
        let master = PtyMaster::open(size).expect("a pty");
        let slave = std::fs::File::from(master.open_slave().expect("the slave opens"));
        let mut termios = rustix::termios::tcgetattr(&slave).expect("termios");
        termios.make_raw();
        rustix::termios::tcsetattr(&slave, rustix::termios::OptionalActions::Now, &termios)
            .expect("the slave goes raw");
        let origin = Instant::now();
        let host = Arc::new(
            PtyHost::start(agent(), master, &cast, size, "xterm-256color", origin)
                .expect("the host starts"),
        );
        if marions_workspace {
            host.license_boot_dialog_answers();
        }
        let records = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&records);
        let inboxes = Arc::new(Inboxes::new(Box::new(move |k| {
            sink.lock().unwrap().push(k);
            Ok(())
        })));
        inboxes.open(&agent());
        let (tx, decisions) = channel();
        let injector = PasteInjector::start_observed(
            agent(),
            &host,
            &inboxes,
            PasteParams {
                settle: BED_SETTLE,
                dialogs,
                ..PasteParams::of(row).expect("a paste row")
            },
            policy,
            tx,
        )
        .expect("the injector starts");
        Bed {
            _dir: dir,
            cast,
            host,
            origin,
            slave,
            inboxes,
            records,
            decisions,
            injector,
        }
    }

    fn node_writes(&mut self, bytes: &[u8]) {
        let before = self.host.bytes_read();
        self.slave.write_all(bytes).unwrap();
        assert!(marion_testsupport::until(
            || self.host.bytes_read() >= before + bytes.len() as u64
        ));
    }

    fn steer(&self, text: &str) -> String {
        self.inboxes
            .enqueue(&agent(), PASTE, Source::Operator, text.into())
            .expect("queued")
    }

    fn read_slave(&mut self, want: usize) -> Vec<u8> {
        read_slave_within(&mut self.slave, want, Duration::from_secs(20))
    }

    /// The next decision the injector reports that is not `want`-irrelevant: skips repeats of
    /// the same hold until `pred` matches, bounded.
    fn await_decision(&self, pred: impl Fn(&Gate) -> bool) -> Gate {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let d = self
                .decisions
                .recv_timeout(left)
                .expect("the injector reported the decision in time");
            if pred(&d) {
                return d;
            }
        }
    }

    fn resolution(&self, id: &str) -> RecordKind {
        let mut found = None;
        assert!(
            marion_testsupport::until(|| {
                found = self.records.lock().unwrap().iter().find_map(|r| match r {
                    RecordKind::MessageDelivered(MessageDelivered { message_id, .. })
                    | RecordKind::MessageDropped(MessageDropped { message_id, .. })
                        if message_id == id =>
                    {
                        Some(r.clone())
                    }
                    _ => None,
                });
                found.is_some()
            }),
            "message {id} was never resolved: {:?}",
            self.records.lock().unwrap()
        );
        found.unwrap()
    }

    fn cast_records(&self) -> Vec<(f64, String, String)> {
        std::fs::read_to_string(&self.cast)
            .unwrap()
            .lines()
            .skip(1)
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

/// **Exactly `want` bytes from a pty slave, or a failure naming what did arrive** once `bound`
/// passes — a paste that never comes fails its test instead of hanging the suite on a blocking
/// read. Shared with the handler's steer tests, which drive the same path through `register_pane`.
pub(crate) fn read_slave_within(
    slave: &mut std::fs::File,
    want: usize,
    bound: Duration,
) -> Vec<u8> {
    let deadline = Instant::now() + bound;
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while got.len() < want {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(
            !left.is_zero(),
            "{} of {want} bytes reached the node within {bound:?}: {:?}",
            got.len(),
            String::from_utf8_lossy(&got)
        );
        let timeout = rustix::event::Timespec {
            tv_sec: i64::try_from(left.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: left.subsec_nanos().into(),
        };
        let mut fds = [rustix::event::PollFd::new(
            &*slave,
            rustix::event::PollFlags::IN,
        )];
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) | Err(rustix::io::Errno::INTR) => continue,
            Ok(_) => {}
            Err(error) => panic!("polling the slave failed: {error}"),
        }
        let n = slave.read(&mut buf).expect("the slave reads");
        assert!(n > 0, "the slave hung up");
        got.extend_from_slice(&buf[..n]);
    }
    got
}

/// The bed's boot settle: short, so a test is not 1.5 s of waiting, and long enough to be seen.
const BED_SETTLE: Duration = Duration::from_millis(150);

/// What a TUI writes as it comes up: bracketed paste on, then a screen with text on it.
const BOOTED: &[u8] = b"\x1b[?2004h\x1b[2J\x1b[H> ready";

fn fast() -> PastePolicy {
    PastePolicy {
        operator_quiet: Duration::from_millis(200),
        paste_mode_grace: Duration::from_secs(30),
        dialog_bound: Duration::from_secs(30),
        recheck: Duration::from_millis(10),
    }
}

fn expected_paste(text: &str) -> Vec<u8> {
    let msg = crate::inbox::Message {
        id: String::new(),
        source: Source::Operator,
        text: text.into(),
        queued_at: std::time::SystemTime::UNIX_EPOCH,
    };
    let mut want = frame(&render(&msg)).into_bytes();
    want.push(b'\r');
    want
}

/// **The end-to-end unit: queued → pasted → delivered.** The node sees exactly one bracketed
/// paste of the rendered message and then the submit; the journal says `pty:paste`; the cast
/// marks the injection with the message id.
#[test]
fn a_queued_message_is_pasted_submitted_and_journaled_as_delivered() {
    let mut bed = Bed::new("paste-delivers", fast());
    bed.node_writes(BOOTED);
    let id = bed.steer("use the v2 API");
    let want = expected_paste("use the v2 API");
    assert_eq!(bed.read_slave(want.len()), want);
    assert_eq!(
        bed.resolution(&id),
        RecordKind::MessageDelivered(MessageDelivered {
            agent_id: agent(),
            message_id: id.clone(),
            via: "pty:paste".into(),
            note: None,
        })
    );
    let records = bed.cast_records();
    assert!(
        records
            .iter()
            .any(|(_, c, d)| c == "m" && d.contains(&id) && d.starts_with("marion")),
        "the cast marks marion's paste: {records:?}"
    );
}

/// **A half-typed line holds the paste until the operator submits it**, and the paste then comes
/// after the operator's line, never inside it.
#[test]
fn the_operators_half_typed_line_holds_the_paste() {
    let mut bed = Bed::new("paste-composer", fast());
    bed.node_writes(BOOTED);
    let lease = bed.host.lease_writer(ConnId(1)).unwrap();
    bed.host.write_opaque_input(&lease, b"hel").unwrap();
    assert_eq!(bed.read_slave(3), b"hel");
    let id = bed.steer("wait for me");
    bed.await_decision(|d| matches!(d, Gate::Wait(Hold::ComposerNotEmpty, _)));
    bed.host.write_opaque_input(&lease, b"lo\r").unwrap();
    let mut want = b"lo\r".to_vec();
    want.extend(expected_paste("wait for me"));
    assert_eq!(
        bed.read_slave(want.len()),
        want,
        "the operator's line, then marion's"
    );
    assert!(matches!(
        bed.resolution(&id),
        RecordKind::MessageDelivered(_)
    ));
}

/// **The operator's last key starts the quiet period**: the paste is written no sooner than
/// `operator_quiet` after it, measured on the cast's own clock. The operator's key leaves no `i`
/// record, so its time is the host's own note of the operator's typing, on the same origin.
#[test]
fn the_paste_waits_for_the_operator_to_be_quiet() {
    let mut bed = Bed::new("paste-quiet", fast());
    bed.node_writes(BOOTED);
    let lease = bed.host.lease_writer(ConnId(1)).unwrap();
    bed.host.write_opaque_input(&lease, b"\r").unwrap();
    bed.read_slave(1);
    let id = bed.steer("after a pause");
    bed.await_decision(|d| matches!(d, Gate::Wait(Hold::OperatorTyping, _)));
    assert!(matches!(
        bed.resolution(&id),
        RecordKind::MessageDelivered(_)
    ));
    let operator_at = bed
        .host
        .input_state()
        .last_operator_input
        .expect("the operator's key was noted")
        .duration_since(bed.origin)
        .as_secs_f64();
    let mut clock = 0.0;
    let mut paste_at = None;
    for (dt, code, _) in bed.cast_records() {
        clock += dt;
        if code == "m" {
            paste_at = Some(clock);
        }
    }
    let gap = paste_at.unwrap() - operator_at;
    // A millisecond of slack for the cast's per-record microsecond rounding, which the host's own
    // `Instant` does not share.
    assert!(
        gap >= 0.199,
        "the paste came {gap}s after the operator's key"
    );
}

/// **No bracketed paste, no paste**: past the grace the message is dropped by name and the node
/// receives nothing — the next byte it reads is the operator's.
#[test]
fn a_terminal_without_bracketed_paste_drops_the_message_by_name() {
    let mut bed = Bed::new(
        "paste-no-2004",
        PastePolicy {
            paste_mode_grace: Duration::ZERO,
            ..fast()
        },
    );
    let id = bed.steer("never typed");
    match bed.resolution(&id) {
        RecordKind::MessageDropped(d) => assert!(d.reason.contains("bracketed paste"), "{d:?}"),
        other => panic!("expected a drop, got {other:?}"),
    }
    let lease = bed.host.lease_writer(ConnId(1)).unwrap();
    bed.host.write_opaque_input(&lease, b"x").unwrap();
    assert_eq!(
        bed.read_slave(1),
        b"x",
        "nothing of the message reached the node"
    );
}

/// A node that turns bracketed paste on within the grace — a TUI still booting — gets the paste.
#[test]
fn a_message_waits_for_the_node_to_turn_bracketed_paste_on() {
    let mut bed = Bed::new("paste-late-2004", fast());
    let id = bed.steer("boot first");
    bed.await_decision(|d| matches!(d, Gate::Wait(Hold::NoBracketedPaste, _)));
    bed.node_writes(BOOTED);
    let want = expected_paste("boot first");
    assert_eq!(bed.read_slave(want.len()), want);
    assert!(matches!(
        bed.resolution(&id),
        RecordKind::MessageDelivered(_)
    ));
}

/// **A TUI that turned bracketed paste on but has drawn nothing is still booting**, and the first
/// paste waits for text on its screen and then `settle` of quiet — copilot 1.0.83 discards a paste
/// typed in that window. Past the first paste, a terminal that just wrote is typed into at once.
#[test]
fn the_first_paste_waits_for_the_tui_to_draw_its_screen_and_go_quiet() {
    let mut bed = Bed::new("paste-boot", fast());
    bed.node_writes(b"\x1b[?2004h\x1b]10;?\x07\x1b[?25l");
    let first = bed.steer("after boot");
    bed.await_decision(|d| matches!(d, Gate::Wait(Hold::Booting, _)));
    bed.node_writes(b"> ready");
    let drawn_at = Instant::now();
    let want = expected_paste("after boot");
    assert_eq!(bed.read_slave(want.len()), want);
    assert!(
        drawn_at.elapsed() >= BED_SETTLE,
        "the paste came {:?} after the screen was drawn",
        drawn_at.elapsed()
    );
    assert!(matches!(
        bed.resolution(&first),
        RecordKind::MessageDelivered(_)
    ));
    // The first message's holds are behind us; only what the second provokes is read below.
    while bed.decisions.try_recv().is_ok() {}

    bed.node_writes(b"busy spinner");
    let second = bed.steer("while busy");
    let want = expected_paste("while busy");
    assert_eq!(bed.read_slave(want.len()), want);
    assert!(matches!(
        bed.resolution(&second),
        RecordKind::MessageDelivered(_)
    ));
    while let Ok(d) = bed.decisions.try_recv() {
        assert!(
            !matches!(d, Gate::Wait(Hold::Booting, _)),
            "a booted terminal is never held for boot again: {d:?}"
        );
    }
}

/// **A provisional composer is not boot on a window-title row** — codex 0.155.1 draws one at once
/// and then sits quiet while it boots, and a paste into it is taken but its Enter dropped. The
/// message is held however long that quiet lasts, and pasted `settle` after the title the real
/// composer sets.
#[test]
fn a_window_title_row_holds_the_first_paste_through_a_quiet_provisional_screen() {
    let mut bed = Bed::with_row("paste-provisional", fast(), TITLED_PASTE, &[], false);
    bed.node_writes(BOOTED);
    let drawn_at = Instant::now();
    let id = bed.steer("after the title");
    // Held well past the settle that would have booted a first-draw row.
    bed.await_decision(|d| {
        matches!(d, Gate::Wait(Hold::Booting, _)) && drawn_at.elapsed() >= BED_SETTLE * 4
    });
    assert!(
        !bed.records.lock().unwrap().iter().any(|r| matches!(
            r,
            RecordKind::MessageDelivered(_) | RecordKind::MessageDropped(_)
        )),
        "nothing was typed into the provisional composer"
    );
    bed.node_writes(b"\x1b]0;repo\x07");
    let titled_at = Instant::now();
    let want = expected_paste("after the title");
    assert_eq!(bed.read_slave(want.len()), want);
    assert!(
        titled_at.elapsed() >= BED_SETTLE,
        "the paste came {:?} after the title",
        titled_at.elapsed()
    );
    assert!(matches!(
        bed.resolution(&id),
        RecordKind::MessageDelivered(_)
    ));
}

/// **A window title that never comes costs no message**: at the grace the message is pasted into
/// the drawn screen and journaled delivered by paste, with a note naming the missing title — a
/// codex whose title is configured off still takes its steers.
#[test]
fn a_window_title_that_never_comes_is_pasted_at_the_grace_and_noted() {
    let policy = PastePolicy {
        paste_mode_grace: Duration::from_millis(600),
        ..fast()
    };
    let mut bed = Bed::with_row("paste-untitled", policy, TITLED_PASTE, &[], false);
    bed.node_writes(BOOTED);
    let id = bed.steer("no title here");
    let asked_at = Instant::now();
    let want = expected_paste("no title here");
    assert_eq!(bed.read_slave(want.len()), want);
    assert!(
        asked_at.elapsed() >= policy.paste_mode_grace,
        "pasted {:?} after it was queued, before the grace",
        asked_at.elapsed()
    );
    let RecordKind::MessageDelivered(delivered) = bed.resolution(&id) else {
        panic!("not delivered: {:?}", bed.records.lock().unwrap())
    };
    assert_eq!(delivered.via, VIA);
    let note = delivered.note.expect("a graced delivery says why");
    assert!(
        note.contains("window title") && note.contains("grace"),
        "{note}"
    );
}

/// **8 KiB — the steer cap — arrives byte-exact** through the whole path.
#[test]
fn a_message_at_the_steer_cap_arrives_byte_exact() {
    let mut bed = Bed::new("paste-8k", fast());
    bed.node_writes(BOOTED);
    let text: String = (0..marion_core::proto::params::MAX_STEER_BYTES)
        .map(|i| (b'a' + (i % 26) as u8) as char)
        .collect();
    let want = expected_paste(&text);
    // Read while the injector writes: 8 KiB is past the pty's queue, so its writer waits on this.
    let id = bed.steer(&text);
    assert_eq!(bed.read_slave(want.len()), want);
    assert!(matches!(
        bed.resolution(&id),
        RecordKind::MessageDelivered(_)
    ));
}

/// Messages go in order, one paste each.
#[test]
fn several_messages_are_pasted_one_at_a_time_in_order() {
    let mut bed = Bed::new("paste-order", fast());
    bed.node_writes(BOOTED);
    let ids: Vec<_> = ["one", "two", "three"]
        .iter()
        .map(|t| bed.steer(t))
        .collect();
    let want: Vec<u8> = ["one", "two", "three"]
        .iter()
        .flat_map(|t| expected_paste(t))
        .collect();
    assert_eq!(bed.read_slave(want.len()), want);
    for id in ids {
        assert!(matches!(
            bed.resolution(&id),
            RecordKind::MessageDelivered(_)
        ));
    }
}

/// **When the node ends, the injector's thread ends**, and a message it was holding for a
/// half-typed line is dropped by name rather than lost.
#[test]
fn the_injector_ends_with_its_node_and_drops_what_it_held() {
    let mut bed = Bed::new("paste-close", fast());
    bed.node_writes(BOOTED);
    let lease = bed.host.lease_writer(ConnId(1)).unwrap();
    bed.host.write_opaque_input(&lease, b"typing").unwrap();
    let id = bed.steer("too late");
    bed.await_decision(|d| matches!(d, Gate::Wait(Hold::ComposerNotEmpty, _)));
    bed.inboxes.close(&agent(), "the node ended");
    assert!(matches!(bed.resolution(&id), RecordKind::MessageDropped(_)));
    assert!(
        bed.injector.join_for_test(Duration::from_secs(10)),
        "the injector's thread did not end with its node"
    );
}

/// claude 2.1.283's first screen, recorded (`tests/fixtures/s37-boot-dialogs/`): folder trust,
/// selection on `No, exit`.
const CLAUDE_TRUST_SCREEN: &[u8] =
    include_bytes!("../../../../tests/fixtures/s37-boot-dialogs/claude-code-2.1.283.raw");

/// **No paste is typed into a trust dialog.** In the operator's own directory marion holds the
/// message until the operator answers the dialog and the composer draws, and only then pastes —
/// the first bytes the node reads are the operator's.
#[test]
fn a_paste_waits_for_the_operator_to_dismiss_a_boot_dialog() {
    let mut bed = Bed::booting("paste-dialog-held", fast(), CLAUDE_LIKE, false);
    bed.node_writes(CLAUDE_TRUST_SCREEN);
    let id = bed.steer("after the dialog");
    bed.await_decision(|d| matches!(d, Gate::Wait(Hold::BootDialog, _)));
    // The launcher is told what the message waits on, so the node's attention item names it.
    assert!(
        marion_testsupport::until_within(Duration::from_secs(5), Duration::from_millis(5), || {
            matches!(bed.host.dialog_hold(), crate::pty::DialogHold::Held(d) if d.needle == TRUST.needle)
        }),
        "the hold was not reported: {:?}",
        bed.host.dialog_hold()
    );
    let lease = bed.host.lease_writer(ConnId(1)).unwrap();
    bed.host.write_opaque_input(&lease, b"\x1b[B\r").unwrap();
    assert_eq!(
        bed.read_slave(4),
        b"\x1b[B\r",
        "the operator's answer, nothing of marion's"
    );
    bed.node_writes(b"\x1b[2J\x1b[H> ");
    let want = expected_paste("after the dialog");
    assert_eq!(bed.read_slave(want.len()), want);
    assert!(matches!(
        bed.resolution(&id),
        RecordKind::MessageDelivered(_)
    ));
    assert_eq!(
        bed.host.dialog_hold(),
        crate::pty::DialogHold::Clear,
        "dismissed, so nothing is held any more"
    );
}

/// **In marion's own workspace the row's keys answer a default-No trust dialog — never a bare
/// CR**, which would quit claude. The keys are written alone, marked in the cast, and the paste
/// follows only once the TUI has redrawn and gone quiet.
#[test]
fn a_default_no_trust_dialog_is_answered_with_the_rows_keys_never_a_bare_cr() {
    let mut bed = Bed::booting("paste-dialog-answered", fast(), CLAUDE_LIKE, true);
    bed.node_writes(CLAUDE_TRUST_SCREEN);
    let id = bed.steer("after the answer");
    assert_eq!(
        bed.read_slave(4),
        b"\x1b[B\r",
        "the row's keys, as one write"
    );
    bed.await_decision(|d| matches!(d, Gate::Wait(Hold::Booting, _)));
    bed.node_writes(b"\x1b[2J\x1b[H> ");
    let drawn_at = Instant::now();
    let want = expected_paste("after the answer");
    assert_eq!(bed.read_slave(want.len()), want);
    assert!(
        drawn_at.elapsed() >= BED_SETTLE,
        "the paste waited for the redraw to settle"
    );
    assert!(matches!(
        bed.resolution(&id),
        RecordKind::MessageDelivered(_)
    ));
    let records = bed.cast_records();
    assert!(
        records
            .iter()
            .any(|(_, c, d)| c == "m" && d.contains("boot dialog") && d.contains(TRUST.needle)),
        "the cast marks marion's answer: {records:?}"
    );
}

/// A codex-like directory-trust dialog: default `1. Yes, continue`, answered by CR.
const CODEX_TRUST: BootDialog = BootDialog {
    needle: "› 1. Yes, continue 2. No, quit",
    action: "trust {repo} once",
    answer: DialogAnswer::Keys(b"\r"),
    note: "test: codex-like directory trust, default Yes",
};
const CODEX_LIKE: &[BootDialog] = &[CODEX_TRUST];

/// **On a window-title row the dialog comes first, then the title, then the paste** — codex
/// 0.155.1 opens on directory trust, then draws its provisional composer, and only its real one
/// sets the title. The dialog is answered with the row's keys alone; the provisional screen that
/// follows, however long it stays quiet, does not open the gate; the paste lands `settle` after
/// the title.
#[test]
fn a_window_title_row_answers_its_dialog_then_waits_for_the_title_before_pasting() {
    let mut bed = Bed::with_row("paste-dialog-title", fast(), TITLED_PASTE, CODEX_LIKE, true);
    bed.node_writes(
        b"\x1b[?2004h\x1b[2J\x1b[HDo you trust the contents of this directory?\r\n\
                      \xe2\x80\xba 1. Yes, continue\r\n  2. No, quit",
    );
    let id = bed.steer("after trust and title");
    assert_eq!(
        bed.read_slave(1),
        b"\r",
        "the row's keys answer the dialog, alone"
    );
    bed.node_writes(b"\x1b[2J\x1b[H> Ask Codex to do anything");
    let drawn_at = Instant::now();
    bed.await_decision(|d| {
        matches!(d, Gate::Wait(Hold::Booting, _)) && drawn_at.elapsed() >= BED_SETTLE * 4
    });
    assert!(
        !bed.records.lock().unwrap().iter().any(|r| matches!(
            r,
            RecordKind::MessageDelivered(_) | RecordKind::MessageDropped(_)
        )),
        "the redrawn provisional composer, quiet and untitled, is not typed into"
    );
    bed.node_writes(b"\x1b]0;repo\x07");
    let titled_at = Instant::now();
    let want = expected_paste("after trust and title");
    assert_eq!(bed.read_slave(want.len()), want);
    assert!(
        titled_at.elapsed() >= BED_SETTLE,
        "the paste came {:?} after the title",
        titled_at.elapsed()
    );
    assert!(matches!(
        bed.resolution(&id),
        RecordKind::MessageDelivered(_)
    ));
}

/// **A dialog marion never answers drops the message by name** past the grace, and the node
/// receives nothing of it — even in marion's own workspace.
#[test]
fn a_dialog_marion_never_answers_drops_the_message_by_name() {
    const HELD_ONLY: &[BootDialog] = &[TRUST_HELD];
    let mut bed = Bed::booting(
        "paste-dialog-dropped",
        PastePolicy {
            paste_mode_grace: Duration::ZERO,
            dialog_bound: Duration::ZERO,
            ..fast()
        },
        HELD_ONLY,
        true,
    );
    bed.node_writes(CLAUDE_TRUST_SCREEN);
    let id = bed.steer("never typed");
    match bed.resolution(&id) {
        RecordKind::MessageDropped(d) => assert!(d.reason.contains("dialog"), "{d:?}"),
        other => panic!("expected a drop, got {other:?}"),
    }
    // Past the grace the launcher is told, and stays told: it ends the node on this.
    assert!(
        matches!(bed.host.dialog_hold(), crate::pty::DialogHold::Expired(d) if d.needle == TRUST_HELD.needle),
        "{:?}",
        bed.host.dialog_hold()
    );
    let lease = bed.host.lease_writer(ConnId(1)).unwrap();
    bed.host.write_opaque_input(&lease, b"x").unwrap();
    assert_eq!(
        bed.read_slave(1),
        b"x",
        "nothing of the message reached the node"
    );
}

/// **A node's dialog hold lasts its own bound; every other boot wait keeps the paste grace.**
#[test]
fn a_nodes_policy_bounds_only_the_dialog_hold_by_the_node() {
    let bound = Duration::from_secs(900);
    let p = PastePolicy::for_node(bound);
    assert_eq!(p.dialog_bound, bound);
    assert_eq!(
        PastePolicy {
            dialog_bound: PastePolicy::PRODUCTION.dialog_bound,
            ..p
        },
        PastePolicy::PRODUCTION
    );
}
