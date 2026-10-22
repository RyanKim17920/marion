//! **Turn delivery into an interactive node: [`TurnDelivery::TerminalPaste`]** — a queued message
//! typed into the node's own terminal, as a bracketed paste and then Enter.
//!
//! This is the generic push for every TUI parent with no notification channel: a pane root, or a
//! native `marion codex|opencode|copilot|…` session, whose supervisor owns the pty master. One
//! [`PasteInjector`] per such node is its inbox's [`DeliveryPort`]; it waits for a moment the
//! paste cannot collide with a person, then types.
//!
//! # When it types (S31, `tests/fixtures/s31-turn-delivery/p0b/tui/`)
//!
//! All three of, read together under the host's write lock ([`PtyHost::inject`]):
//!
//! * **The node asked for bracketed paste** (`CSI ? 2004 h`). Every measured TUI does at boot. An
//!   unbracketed newline would submit half the message, and codex turns an unbracketed burst's CR
//!   into a newline, so a paste is *never* sent without it: the message waits
//!   [`PastePolicy::paste_mode_grace`] for the mode (a TUI still booting), then is dropped by name.
//! * **The operator's composer is presumed empty** — their last key was Enter, or they have typed
//!   nothing. A paste into a half-typed line would submit both as one turn.
//! * **The operator has been quiet for [`PastePolicy::operator_quiet`]** (1.5 s): a person
//!   mid-thought is not interrupted by text appearing under their cursor.
//!
//! And before the injector's **first** paste into a terminal, one more: **the TUI has booted** —
//! it has shown the row's [`BootSignal`] since turning bracketed paste on (drawn text, or set its
//! window title), and then been quiet for the row's [`IdleSignal`] ([`Hold::Booting`]).
//!
//! # Boot dialogs (S37, `tests/fixtures/s37-boot-dialogs/`)
//!
//! Before that first paste, too: **no dialog the row names is on the screen**
//! ([`marion_harness::spec::BootDialog`], read by the host as text drawn since anyone last typed).
//! A TUI in a fresh directory opens on folder trust, and a paste typed into it answers it: claude
//! 2.1.283's defaults to `No, exit`, so paste + CR quits the session; codex 0.155.1's swallows the
//! paste. Where the row marks the dialog answerable **and** the host was licensed — for folder
//! trust, the child runs in a worktree marion created; for first-run setup, marion owns the
//! harness config ([`crate::pty::PtyHost::license_boot_dialog_answers`], [`Answers`](crate::paste::Answers)) — the
//! injector types the row's keys alone ([`Gate::Answer`]), once per dialog, and waits for the TUI
//! to redraw and settle before pasting. A dialog whose answer only declines and records nothing
//! ([`marion_harness::spec::DialogAnswer::Decline`]: codex's `Open restricted` for a checkout
//! marion pinned untrusted) needs no license. Anywhere else, and for a dialog the row holds, it waits
//! ([`Hold::BootDialog`]) for someone to dismiss it, and past the grace drops the message by name.
//!
//! # Asks (2026-10-16, `tests/fixtures/pane-asks/`)
//!
//! And before **every** paste, booted or not: **no ask the row names is on the screen**
//! ([`marion_harness::spec::BootDialogs::asks`]) — an approval or permission prompt the TUI raises
//! mid-session. A live steer into a codex 0.160.1 pane sitting on `Would you like to run the
//! following command? › 1. Yes, proceed` lost its text and its Enter approved the command. An ask
//! is never answered by marion and never refused for: the message waits ([`Hold::Ask`]) for as
//! long as the prompt is up — its footer, or its header with an option line, on the screen as it
//! stands now (the header alone is a sentence an idle reply can end on) — and the node's
//! launcher puts it on the operator's attention queue meanwhile. A key does not end the hold (an
//! arrow moves the selection and leaves the prompt up); the prompt leaving the screen does.
//!
//! # Why the row's [`IdleSignal`] is waited on once, not before every paste
//!
//! The row carries `OutputQuiet{1500}` — the measured "the TUI is idle" signal. Past boot this
//! driver deliberately does **not** wait for it. S31 measured Enter while busy on all four TUIs
//! (codex, opencode, copilot, claude): never dropped, never an interrupt, delivered right after the
//! in-flight model response (codex "submitted after next tool call", claude "steer in real-time",
//! copilot "ctrl+q enqueue", opencode a pending message). Writing while busy is therefore safe on
//! every measured row, and waiting for output quiet would only add latency — and never fire on a
//! TUI that repaints a clock.
//!
//! Boot is the exception, measured in `native_facade_e2e.rs`: copilot 1.0.83 turns bracketed paste
//! on in its first write, then (with no terminal answering its colour queries) draws nothing for
//! twenty seconds, and a paste typed in that window is **discarded** — journaled delivered, never
//! submitted. "Drawn, then quiet" is the moment its composer exists; bracketed paste alone is not.
//! The match in [`PasteParams::of`] names the signal so a new variant has to decide here.
//!
//! Drawn is not always enough: codex 0.155.1 draws a **provisional composer** at once and then
//! sits quiet while its app server boots — 1.25 s unloaded, well past 1.5 s on a loaded machine.
//! That composer takes a paste and drops the Enter, so the message sat in the real composer
//! unsubmitted (`native_facade_e2e`'s codex lane, 3 of 20 runs under load). Its row's
//! [`BootSignal::WindowTitle`] counts the quiet from the first window title instead, which only
//! the real composer sets — and a screen drawn since, so a boot dialog marion answered is still
//! followed by a redraw. The boot dialog wait comes first: no mark opens the gate while a dialog
//! is on the screen. A codex whose title is configured off (`tui.terminal_title = []`) never shows
//! the mark; a mark the operator can switch off ([`BootSignal::operator_can_switch_off`]) that has
//! not come by the grace is not taken as a TUI still booting — a provisional composer lasts
//! seconds — so the message is pasted on the drawn screen at the grace ([`Gate::Graced`]) and
//! journaled delivered with a note saying so. A preference never costs a message; a dialog still
//! holds it.
//!
//! # A cancel
//!
//! A node being cancelled whose row's interactive verb is [`AbortVerb::Keys`] has those keys typed
//! into its terminal, `gap_ms` apart, **past every hold above**: a cancel is the operator's own
//! request to end the turn, so a key they are typing or a dialog on the screen does not stand in
//! its way. The message held for a paste is dropped as cancelled, and the injector types nothing
//! more. A row whose verb is anything else gets `false` from the port, and the canceller kills.
//!
//! # What is journaled
//!
//! `MessageDelivered { via: "pty:paste" }` once the paste and its submit reached the master, or
//! `MessageDropped` with the reason. The cast carries the paste itself: an `m` marker naming the
//! message id, then the `i` records — the recording says what marion typed and that marion typed it.
//!
//! [`TurnDelivery::TerminalPaste`]: marion_harness::spec::TurnDelivery::TerminalPaste
//! [`PasteInjector`]: crate::paste::PasteInjector
//! [`DeliveryPort`]: crate::inbox::DeliveryPort
//! [`PtyHost::inject`]: crate::pty::PtyHost::inject
//! [`PastePolicy::paste_mode_grace`]: crate::paste::PastePolicy::paste_mode_grace
//! [`PastePolicy::operator_quiet`]: crate::paste::PastePolicy::operator_quiet
//! [`BootSignal`]: marion_harness::spec::BootSignal
//! [`IdleSignal`]: marion_harness::spec::IdleSignal
//! [`Hold::Booting`]: crate::paste::Hold::Booting
//! [`Gate::Answer`]: crate::paste::Gate::Answer
//! [`Hold::BootDialog`]: crate::paste::Hold::BootDialog
//! [`Hold::Ask`]: crate::paste::Hold::Ask
//! [`PasteParams::of`]: crate::paste::PasteParams::of
//! [`BootSignal::WindowTitle`]: marion_harness::spec::BootSignal::WindowTitle
//! [`BootSignal::operator_can_switch_off`]: marion_harness::spec::BootSignal::operator_can_switch_off
//! [`Gate::Graced`]: crate::paste::Gate::Graced
//! [`AbortVerb::Keys`]: marion_harness::spec::AbortVerb::Keys

use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_harness::spec::{
    AbortVerb, BootDialog, BootSignal, DialogAnswer, HarnessSpec, IdleSignal, NodeShape,
    TurnDelivery,
};

use crate::inbox::{DeliveryPort, Inboxes, Message, render};
use crate::pty::{DialogHold, Injected, Injection, InputState, PtyHost};

/// The lane's word on `MessageDelivered`.
pub const VIA: &str = "pty:paste";

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// **One message as one bracketed paste.** Inside the brackets only text survives: every control
/// character except newline and tab is removed (an `ESC` could close the paste early — a child's
/// result is quoted here, and its `ESC[201~` would turn the rest into typing, its first newline a
/// submit), and a CR, alone or before LF, becomes the newline it means.
pub fn frame(text: &str) -> String {
    format!(
        "{PASTE_START}{}{PASTE_END}",
        crate::printable::printable(text)
    )
}

/// The row's paste, as this driver uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteParams {
    pub submit: &'static [u8],
    pub submit_delay: Duration,
    /// How long the terminal must be quiet, after drawing its first screen, before the first
    /// paste — the row's [`IdleSignal`], used for boot only. See the module doc.
    pub settle: Duration,
    /// What marks the row's TUI as booted, before the settle is counted ([`BootSignal`]).
    pub boot_mark: BootSignal,
    /// The dialogs the row's TUI can show before its composer ([`HarnessSpec::boot_dialogs`]):
    /// the first paste is never typed into one.
    pub dialogs: &'static [BootDialog],
    /// The asks it can raise mid-session ([`marion_harness::spec::BootDialogs::asks`]): no paste
    /// is ever typed into one.
    pub asks: &'static [marion_harness::spec::Ask],
    /// The keys that end the running turn when the node is cancelled, and the gap between them —
    /// the row's interactive [`AbortVerb::Keys`], or `None` for a row with no such verb.
    pub abort: Option<AbortKeys>,
}

/// A row's [`AbortVerb::Keys`], as this driver types it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbortKeys {
    pub keys: &'static [&'static [u8]],
    pub gap: Duration,
}

impl AbortKeys {
    /// The keys a verb types, or `None` for a verb that is not keys.
    pub fn of(verb: AbortVerb) -> Option<AbortKeys> {
        match verb {
            AbortVerb::Keys { keys, gap_ms, .. } => Some(AbortKeys {
                keys,
                gap: Duration::from_millis(u64::from(gap_ms)),
            }),
            AbortVerb::Channel { .. } | AbortVerb::None { .. } => None,
        }
    }
}

impl PasteParams {
    /// The paste a row states, or `None` for every other strategy — only a `TerminalPaste` row gets
    /// an injector.
    pub fn of(delivery: TurnDelivery) -> Option<PasteParams> {
        let TurnDelivery::TerminalPaste {
            idle,
            boot,
            submit,
            submit_delay_ms,
            note: _,
        } = delivery
        else {
            return None;
        };
        // Waited on before the first paste only: every measured TUI queues or steers input
        // written while it is busy, but one discards input typed while it boots. See the module
        // doc.
        let settle = match idle {
            IdleSignal::OutputQuiet { ms } => Duration::from_millis(u64::from(ms)),
        };
        Some(PasteParams {
            submit,
            submit_delay: Duration::from_millis(u64::from(submit_delay_ms)),
            settle,
            boot_mark: boot,
            dialogs: &[],
            asks: &[],
            abort: None,
        })
    }

    /// What ends the node's boot where no paste does: the mark and settle a first paste waits for.
    pub fn boot(&self) -> crate::pty::BootMark {
        crate::pty::BootMark {
            mark: self.boot_mark,
            settle: self.settle,
        }
    }

    /// **A row's paste, whole**: its interactive delivery, if that is a paste, with the dialogs
    /// its TUI shows at boot and the keys its cancel types.
    pub fn for_row(row: &HarnessSpec) -> Option<PasteParams> {
        let params = PasteParams::of(marion_harness::spec::delivery_for(
            row,
            NodeShape::Interactive,
        ))?;
        Some(PasteParams {
            dialogs: row.boot_dialogs.dialogs,
            asks: row.boot_dialogs.asks,
            abort: AbortKeys::of(marion_harness::spec::abort_for(row, NodeShape::Interactive)),
            ..params
        })
    }
}

/// What the first paste into a terminal also waits for: the row's settle time, and whether a
/// boot dialog on the screen may be answered ([`crate::pty::PtyHost::answers_boot_dialogs`], and
/// not already answered once — a dialog that is back after marion's keys is held).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boot {
    pub settle: Duration,
    pub mark: BootSignal,
    pub answer_dialogs: Answers,
}

/// **Which of a row's answerable dialogs marion may answer on this terminal**: folder trust
/// ([`DialogAnswer::Keys`]) only in a workspace marion created where the answer stays out of the
/// operator's config; first-run setup ([`DialogAnswer::Setup`]) wherever marion owns the config;
/// a dialog whose answer only declines and records nothing ([`DialogAnswer::Decline`]) on every
/// terminal, which is why a host starts with [`Answers::DECLINE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Answers {
    pub workspace: bool,
    pub config: bool,
    pub decline: bool,
}

impl Answers {
    pub const NONE: Answers = Answers {
        workspace: false,
        config: false,
        decline: false,
    };
    /// What every host may answer before, and whatever, it is licensed for.
    pub const DECLINE: Answers = Answers {
        decline: true,
        ..Answers::NONE
    };
    pub const ALL: Answers = Answers {
        workspace: true,
        config: true,
        decline: true,
    };
}

/// What marion may answer now: what the host was licensed for, unless the dialog on the screen is
/// one marion already answered — back after its keys, it is held. Each dialog is answered once, so
/// a chain of setup screens is walked one answer each.
pub fn licence(
    granted: Answers,
    showing: Option<&'static BootDialog>,
    answered: &[&'static str],
) -> Answers {
    match showing {
        Some(d) if answered.contains(&d.needle) => Answers::NONE,
        _ => granted,
    }
}

/// Marion's side of when a paste may be typed — not measured per row, because it is about the
/// person at the terminal, not the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PastePolicy {
    /// How long since the operator's last key before marion types. 1.5 s: past a typing pause,
    /// short of a noticeable delivery delay.
    pub operator_quiet: Duration,
    /// How long a message waits for the node to be pasteable — bracketed paste on and, before
    /// the first paste, booted — before it is dropped. 30 s covers every measured TUI's boot; past
    /// it the node is not a terminal marion can paste into safely, and saying so beats holding the
    /// message for the node's whole life.
    pub paste_mode_grace: Duration,
    /// How long a message waits on a boot dialog marion may not answer before it is dropped. Not
    /// the paste grace: the hold is on the operator's attention queue (the launcher journals it),
    /// so it lasts as long as the node may — its own wall-clock bound ([`Self::for_node`]).
    pub dialog_bound: Duration,
    /// How often a held message looks again. The holds end on state marion is not told about (a
    /// key, a mode), so they are re-read — only while a message waits.
    pub recheck: Duration,
    /// How long a message held on state marion **is** told about — an ask leaving the screen
    /// rings the host's `dialogs_changed`, which wakes the driver — waits before it looks anyway.
    /// The safety path, not the mechanism.
    pub backstop: Duration,
}

impl PastePolicy {
    pub const PRODUCTION: PastePolicy = PastePolicy {
        operator_quiet: Duration::from_millis(1500),
        paste_mode_grace: Duration::from_secs(30),
        dialog_bound: Duration::from_secs(30),
        recheck: Duration::from_millis(100),
        backstop: Duration::from_secs(30),
    };

    /// [`Self::PRODUCTION`] for a node whose wall-clock bound is `bound`: a boot dialog it is held
    /// on waits for the operator that long.
    pub const fn for_node(bound: Duration) -> PastePolicy {
        PastePolicy {
            dialog_bound: bound,
            ..Self::PRODUCTION
        }
    }
}

/// Why a paste is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    OperatorTyping,
    ComposerNotEmpty,
    NoBracketedPaste,
    /// Before the first paste: the TUI has not yet shown the row's boot mark under bracketed
    /// paste and then gone quiet for the row's settle time.
    Booting,
    /// Before the first paste: one of the row's boot dialogs is on the screen, and a paste would
    /// answer it.
    BootDialog,
    /// One of the row's asks (an approval prompt) is on the screen, and a paste's submit would
    /// answer it. Held for as long as it is up, never refused for it: the node is on its
    /// operator's attention queue meanwhile, and the message is theirs to have waited for.
    Ask,
}

/// One look at the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Ready,
    /// Paste now, without the row's boot mark: it is one the operator can switch off and the
    /// grace ran out on a drawn screen. The text is the journal note for the delivery.
    Graced(String),
    /// Not a paste yet: answer this boot dialog with its row's keys first.
    Answer(&'static BootDialog),
    /// Not now; look again at the instant.
    Wait(Hold, Instant),
    /// Never, for this message.
    Refuse(String),
}

/// **The decision**, on values: `state` as of `now`. `boot` is what the first paste also waits for
/// while the injector has not yet pasted into this terminal ([`Boot`]), `None` after. `unready_since` is the grace's
/// clock — set the first time the terminal itself (its paste mode, its boot) was the only thing in
/// the way, cleared when it is ready or a message is refused for it.
pub fn gate(
    state: &InputState,
    now: Instant,
    policy: &PastePolicy,
    boot: Option<Boot>,
    unready_since: &mut Option<Instant>,
) -> Gate {
    if !state.composer_empty {
        return Gate::Wait(Hold::ComposerNotEmpty, now + policy.recheck);
    }
    // **Before anything else about the terminal**, booted or not: typed into an approval prompt,
    // a paste's text is lost and its submit approves (codex 0.160.1's `› 1. Yes, proceed`). The
    // grace's clock is left alone — an ask is not the terminal failing to be pasteable.
    if state.ask.is_some() {
        // The prompt leaving the screen wakes the driver; the look below is the backstop.
        return Gate::Wait(Hold::Ask, now + policy.backstop);
    }
    if let Some(typed) = state.last_operator_input {
        let quiet_at = typed + policy.operator_quiet;
        if now < quiet_at {
            return Gate::Wait(Hold::OperatorTyping, quiet_at);
        }
    }
    let dialog = boot.and(state.boot_dialog);
    let (hold, look) = match (dialog, state.bracketed_paste, boot) {
        // Asked before the paste mode: the answer is keys, not a paste, and a dialog can be up
        // before the TUI turns bracketed paste on.
        (Some(d), _, Some(b)) => match d.answer {
            DialogAnswer::Keys(_) if b.answer_dialogs.workspace => {
                *unready_since = None;
                return Gate::Answer(d);
            }
            DialogAnswer::Setup(_) if b.answer_dialogs.config => {
                *unready_since = None;
                return Gate::Answer(d);
            }
            DialogAnswer::Decline(_) if b.answer_dialogs.decline => {
                *unready_since = None;
                return Gate::Answer(d);
            }
            _ => (Hold::BootDialog, now + policy.recheck),
        },
        (_, false, _) => (Hold::NoBracketedPaste, now + policy.recheck),
        (_, true, None) => {
            *unready_since = None;
            return Gate::Ready;
        }
        (_, true, Some(Boot { settle, mark, .. })) => {
            let marked = match mark {
                BootSignal::FirstDraw => state.screen_drawn,
                BootSignal::WindowTitle => state.titled && state.screen_drawn,
            };
            match state.last_output.map(|at| at + settle) {
                Some(quiet_at) if marked && now >= quiet_at => {
                    *unready_since = None;
                    return Gate::Ready;
                }
                Some(quiet_at) if marked => (Hold::Booting, quiet_at),
                _ => (Hold::Booting, now + policy.recheck),
            }
        }
    };
    let since = *unready_since.get_or_insert(now);
    let bound = match hold {
        Hold::BootDialog => policy.dialog_bound,
        _ => policy.paste_mode_grace,
    };
    if now.saturating_duration_since(since) >= bound {
        *unready_since = None;
        let grace = bound.as_secs();
        let mark = boot.map_or(BootSignal::FirstDraw, |b| b.mark);
        if hold == Hold::Booting && mark.operator_can_switch_off() && state.screen_drawn {
            return Gate::Graced(format!(
                "the node never {} within {grace} s — a mark its own configuration can switch \
                 off — so the message was pasted on its drawn screen at the grace",
                mark.describe()
            ));
        }
        return Gate::Refuse(match (hold, dialog) {
            (Hold::BootDialog, Some(d)) => format!(
                "the node's terminal showed a dialog before its composer (`{}`: {}) for {grace} s \
                 and nobody dismissed it; marion does not answer it here, and a paste typed into \
                 it would answer it",
                d.needle, d.note
            ),
            (Hold::Booting, _) => {
                let mark = mark.describe();
                format!(
                    "the node's terminal did not finish booting within {grace} s — it never \
                     {mark} under bracketed paste and then went quiet — and a paste typed into a \
                     TUI that is still booting is discarded"
                )
            }
            _ => format!(
                "the node's terminal has not enabled bracketed paste (DECSET 2004) for {grace} s, \
                 and marion never types a message unbracketed: a newline in it would submit part \
                 of it"
            ),
        });
    }
    Gate::Wait(hold, look)
}

#[derive(Debug, Default)]
struct Signal {
    woken: bool,
    closed: bool,
    /// The node is being cancelled: type the row's abort keys, then nothing more.
    aborting: bool,
}

/// **A `TerminalPaste` node's delivery port**: a thread that takes the node's queued messages one
/// at a time and pastes each when [`gate`] opens. It holds the host and the inboxes weakly, so it
/// never keeps a finished pane alive, and it ends when the inbox tells it the node closed.
pub struct PasteInjector {
    signal: Mutex<Signal>,
    wake: Condvar,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// The row states abort keys ([`PasteParams::abort`]); without them a cancel is the
    /// canceller's kill, and the port says so.
    aborts: bool,
}

/// The host's `dialogs_changed` wakes the driver as a queued message does: a held message looks
/// again the moment what the screen shows changes.
impl crate::wake::Waker for PasteInjector {
    fn wake(&self) {
        DeliveryPort::wake(self);
    }
}

impl DeliveryPort for PasteInjector {
    fn wake(&self) {
        self.lock().woken = true;
        self.wake.notify_all();
    }

    fn closed(&self) {
        self.lock().closed = true;
        self.wake.notify_all();
    }

    fn abort(&self) -> bool {
        if !self.aborts {
            return false;
        }
        self.lock().aborting = true;
        self.wake.notify_all();
        true
    }
}

struct Worker {
    agent: AgentId,
    host: Weak<PtyHost>,
    inboxes: Weak<Inboxes>,
    params: PasteParams,
    policy: PastePolicy,
    port: Arc<PasteInjector>,
    observer: Option<Sender<Gate>>,
}

impl PasteInjector {
    /// Start `agent`'s injector and attach it to its inbox. `None` when the inbox is not open (the
    /// thread is released at once) or the thread could not be started.
    pub fn start(
        agent: AgentId,
        host: &Arc<PtyHost>,
        inboxes: &Arc<Inboxes>,
        params: PasteParams,
        policy: PastePolicy,
    ) -> Option<Arc<PasteInjector>> {
        Self::spawn(agent, host, inboxes, params, policy, None)
    }

    #[cfg(test)]
    fn start_observed(
        agent: AgentId,
        host: &Arc<PtyHost>,
        inboxes: &Arc<Inboxes>,
        params: PasteParams,
        policy: PastePolicy,
        observer: Sender<Gate>,
    ) -> Option<Arc<PasteInjector>> {
        Self::spawn(agent, host, inboxes, params, policy, Some(observer))
    }

    fn spawn(
        agent: AgentId,
        host: &Arc<PtyHost>,
        inboxes: &Arc<Inboxes>,
        params: PasteParams,
        policy: PastePolicy,
        observer: Option<Sender<Gate>>,
    ) -> Option<Arc<PasteInjector>> {
        let port = Arc::new(PasteInjector {
            signal: Mutex::new(Signal::default()),
            wake: Condvar::new(),
            thread: Mutex::new(None),
            aborts: params.abort.is_some(),
        });
        let worker = Worker {
            agent: agent.clone(),
            host: Arc::downgrade(host),
            inboxes: Arc::downgrade(inboxes),
            params,
            policy,
            port: Arc::clone(&port),
            observer,
        };
        let thread = match std::thread::Builder::new()
            .name("marion-paste".into())
            .spawn(move || worker.run())
        {
            Ok(thread) => thread,
            Err(error) => {
                eprintln!(
                    "marion: no terminal-paste delivery for {}: its thread did not start: {error}",
                    agent.0
                );
                return None;
            }
        };
        *port.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(thread);
        if inboxes.attach_port(&agent, Arc::clone(&port) as Arc<dyn DeliveryPort>) {
            Some(port)
        } else {
            port.closed();
            None
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Signal> {
        self.signal.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wait up to `bound` for the thread to end.
    #[cfg(test)]
    fn join_for_test(&self, bound: Duration) -> bool {
        let Some(thread) = self.thread.lock().unwrap().take() else {
            return true;
        };
        let finished = marion_testsupport::until_within(bound, Duration::from_millis(5), || {
            thread.is_finished()
        });
        finished && thread.join().is_ok()
    }
}

impl Worker {
    fn run(self) {
        // A message taken from the inbox that the operator beat to the terminal: it goes first.
        let mut held: Option<Message> = None;
        let mut unready_since = None;
        // Until the first paste lands, the terminal must also have booted.
        let mut booted = false;
        // Each boot dialog is answered once: one that is back after marion's keys is held.
        let mut answered: Vec<&'static str> = Vec::new();
        if let Some(host) = self.host.upgrade() {
            host.watch_dialogs(
                self.params.dialogs,
                self.params.asks,
                Some(self.params.boot()),
            );
            host.dialogs_changed().attach(&self.port);
        }
        let mut next_look: Option<Instant> = None;
        while self.await_work(next_look.take()) {
            let (Some(host), Some(inboxes)) = (self.host.upgrade(), self.inboxes.upgrade()) else {
                break;
            };
            if std::mem::take(&mut self.port.lock().aborting) {
                self.type_abort(&host);
                if let Some(m) = held.take() {
                    inboxes.dropped(&self.agent, &m.id, crate::inbox::CANCELLED_BEFORE_DELIVERY);
                }
                break;
            }
            if held.is_none() && inboxes.queued(&self.agent) == 0 {
                continue;
            }
            // One read of the screen for the licence, the gate and the hold: read apart, a dialog
            // drawn between them would be licensed as one screen and gated as another.
            let state = host.input_state();
            let boot = (!booted).then(|| Boot {
                settle: self.params.settle,
                mark: self.params.boot_mark,
                answer_dialogs: licence(host.answers_boot_dialogs(), state.boot_dialog, &answered),
            });
            let decision = gate(
                &state,
                Instant::now(),
                &self.policy,
                boot,
                &mut unready_since,
            );
            self.observe(&decision);
            // Tell the launcher what the first message waits on, where it is a boot dialog marion
            // does not answer: it raises the node's attention item and, past the grace, ends it.
            // `Expired` is final: the launcher ends the node on it.
            let showing = state.boot_dialog;
            let was = host.dialog_hold();
            host.set_dialog_hold(match (&decision, showing) {
                _ if matches!(was, DialogHold::Expired(_)) => was,
                (Gate::Wait(Hold::BootDialog, _), Some(d)) => DialogHold::Held(d),
                (Gate::Refuse(_), Some(d)) if boot.is_some() => DialogHold::Expired(d),
                // Held on something else (the operator typing) with the dialog still up.
                (Gate::Wait(..), Some(_)) => was,
                _ => DialogHold::Clear,
            });
            next_look = Some(match &decision {
                Gate::Wait(_, at) => *at,
                Gate::Answer(dialog) => {
                    if self.answer(&host, &inboxes, &mut held, dialog, boot) {
                        answered.push(dialog.needle);
                    }
                    // The TUI redraws after the keys; the next look waits for that as a boot.
                    Instant::now() + self.policy.recheck
                }
                Gate::Refuse(reason) => {
                    if let Some(m) = held.take().or_else(|| inboxes.take_next(&self.agent)) {
                        inboxes.dropped(&self.agent, &m.id, reason);
                    }
                    // The next message, if any, is looked at now.
                    Instant::now()
                }
                Gate::Ready | Gate::Graced(_) => {
                    let note = match &decision {
                        Gate::Graced(note) => Some(note.as_str()),
                        _ => None,
                    };
                    match held.take().or_else(|| inboxes.take_next(&self.agent)) {
                        Some(m) => {
                            held = self.paste(&host, &inboxes, m, boot, note);
                            if !booted && held.is_none() {
                                booted = true;
                                host.boot_over();
                            }
                            if held.is_some() {
                                // The operator beat a graced paste to the terminal: the grace
                                // has still run out, and the next look keeps it so.
                                if note.is_some() {
                                    unready_since = Some(self.graced_at(Instant::now()));
                                }
                                Instant::now() + self.policy.recheck
                            } else {
                                Instant::now()
                            }
                        }
                        None => Instant::now(),
                    }
                }
            });
        }
        if let (Some(m), Some(inboxes)) = (held, self.inboxes.upgrade()) {
            inboxes.dropped(
                &self.agent,
                &m.id,
                "the node ended before its next turn took the message",
            );
        }
    }

    /// **Answer `dialog` with its row's keys**, alone — no paste rides with them. `true` once they
    /// reached the terminal. A write that fails drops the message waiting on the dialog, by name.
    fn answer(
        &self,
        host: &PtyHost,
        inboxes: &Inboxes,
        held: &mut Option<Message>,
        dialog: &'static BootDialog,
        boot: Option<Boot>,
    ) -> bool {
        let Some(keys) = dialog.answer.keys() else {
            return false;
        };
        let label = format!("marion: boot dialog answered: {}", dialog.needle);
        let injection = Injection {
            label: &label,
            body: keys,
            submit: b"",
            submit_delay: Duration::ZERO,
        };
        let admit = |state: &InputState| {
            gate(state, Instant::now(), &self.policy, boot, &mut None) == Gate::Answer(dialog)
        };
        match host.inject(&injection, &admit) {
            Ok(Injected::Written) => true,
            // An answer has no submit to withhold.
            Ok(Injected::Declined | Injected::Unsubmitted) => false,
            Err(error) => {
                if let Some(m) = held.take().or_else(|| inboxes.take_next(&self.agent)) {
                    inboxes.dropped(
                        &self.agent,
                        &m.id,
                        &format!(
                            "the node's terminal showed a boot dialog ({}) and marion's answer \
                             could not be written to it: {error}",
                            dialog.needle
                        ),
                    );
                }
                false
            }
        }
    }

    /// **Type the row's abort keys**, `gap` apart, past every hold ([`gate`] is not asked): the
    /// cancel is the operator's own request. Best-effort — a terminal that cannot be written is one
    /// the canceller's kill ends.
    fn type_abort(&self, host: &PtyHost) {
        let Some(abort) = self.params.abort else {
            return;
        };
        for (i, key) in abort.keys.iter().enumerate() {
            if i > 0 {
                std::thread::sleep(abort.gap);
            }
            let injection = Injection {
                label: "marion: cancel",
                body: key,
                submit: b"",
                submit_delay: Duration::ZERO,
            };
            if host.inject(&injection, &|_| true).is_err() {
                return;
            }
        }
    }

    /// A grace clock that has run out as of `now`.
    fn graced_at(&self, now: Instant) -> Instant {
        now.checked_sub(self.policy.paste_mode_grace).unwrap_or(now)
    }

    /// Paste `m`, or hand it back when the operator got to the terminal first. `note`, set for a
    /// [`Gate::Graced`] paste, rides the delivery record.
    fn paste(
        &self,
        host: &PtyHost,
        inboxes: &Inboxes,
        m: Message,
        boot: Option<Boot>,
        note: Option<&str>,
    ) -> Option<Message> {
        let body = frame(&render(&m));
        let label = format!("marion: turn delivery {}", m.id);
        let injection = Injection {
            label: &label,
            body: body.as_bytes(),
            submit: self.params.submit,
            submit_delay: self.params.submit_delay,
        };
        // Asked again under the write lock: the look above was without it, and a key may have
        // landed since. The grace clock is not advanced here — a mode that went off in between is
        // the next look's to count.
        // A graced paste is asked with the grace already run out.
        let admit = |state: &InputState| {
            let now = Instant::now();
            let mut clock = note.map(|_| self.graced_at(now));
            matches!(
                gate(state, now, &self.policy, boot, &mut clock),
                Gate::Ready | Gate::Graced(_)
            )
        };
        match host.inject(&injection, &admit) {
            Ok(Injected::Written) => {
                inboxes.delivered_noting(&self.agent, &m.id, VIA, note);
                None
            }
            Ok(Injected::Declined) => Some(m),
            Ok(Injected::Unsubmitted) => {
                inboxes.dropped(
                    &self.agent,
                    &m.id,
                    "an approval prompt came up between the paste and its submit; the submit was \
                     withheld so it would not answer the prompt, and the pasted text may be lost",
                );
                None
            }
            Err(error) => {
                inboxes.dropped(
                    &self.agent,
                    &m.id,
                    &format!("the paste could not be written to the node's terminal: {error}"),
                );
                None
            }
        }
    }

    /// Block until there may be work: a wake, or the look `at` coming due; with no look due, until
    /// the next wake. `false` once the port is closed.
    fn await_work(&self, at: Option<Instant>) -> bool {
        let mut signal = self.port.lock();
        loop {
            if signal.closed {
                return false;
            }
            // Left set: the loop takes it.
            if signal.aborting {
                return true;
            }
            if signal.woken {
                signal.woken = false;
                return true;
            }
            signal = match at {
                Some(at) => {
                    let now = Instant::now();
                    if now >= at {
                        return true;
                    }
                    self.port
                        .wake
                        .wait_timeout(signal, at - now)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
                None => self
                    .port
                    .wake
                    .wait(signal)
                    .unwrap_or_else(|e| e.into_inner()),
            };
        }
    }

    fn observe(&self, decision: &Gate) {
        if let Some(observer) = &self.observer {
            let _ = observer.send(decision.clone());
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
