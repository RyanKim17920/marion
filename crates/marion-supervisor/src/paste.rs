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
//! paste. Where the row marks the dialog answerable **and** the host was licensed — the child runs
//! in a worktree marion created ([`crate::pty::PtyHost::license_boot_dialog_answers`]) — the
//! injector types the row's keys alone ([`Gate::Answer`]), once, and waits for the TUI to redraw
//! and settle before pasting. Anywhere else, and for a dialog the row holds, it waits
//! ([`Hold::BootDialog`]) for someone to dismiss it, and past the grace drops the message by name.
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
//! # What is journaled
//!
//! `MessageDelivered { via: "pty:paste" }` once the paste and its submit reached the master, or
//! `MessageDropped` with the reason. The cast carries the paste itself: an `m` marker naming the
//! message id, then the `i` records — the recording says what marion typed and that marion typed it.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_harness::spec::{
    BootDialog, BootSignal, DialogAnswer, HarnessSpec, IdleSignal, NodeShape, TurnDelivery,
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
        })
    }

    /// **A row's paste, whole**: its interactive delivery, if that is a paste, with the dialogs
    /// its TUI shows at boot.
    pub fn for_row(row: &HarnessSpec) -> Option<PasteParams> {
        let params = PasteParams::of(marion_harness::spec::delivery_for(
            row,
            NodeShape::Interactive,
        ))?;
        Some(PasteParams {
            dialogs: row.boot_dialogs.dialogs,
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
    pub answer_dialogs: bool,
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
}

impl PastePolicy {
    pub const PRODUCTION: PastePolicy = PastePolicy {
        operator_quiet: Duration::from_millis(1500),
        paste_mode_grace: Duration::from_secs(30),
        dialog_bound: Duration::from_secs(30),
        recheck: Duration::from_millis(100),
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
            DialogAnswer::Keys(_) if b.answer_dialogs => {
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
}

/// **A `TerminalPaste` node's delivery port**: a thread that takes the node's queued messages one
/// at a time and pastes each when [`gate`] opens. It holds the host and the inboxes weakly, so it
/// never keeps a finished pane alive, and it ends when the inbox tells it the node closed.
pub struct PasteInjector {
    signal: Mutex<Signal>,
    wake: Condvar,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
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
        // A boot dialog is answered once: one that is back after marion's keys is held.
        let mut answered = false;
        if let Some(host) = self.host.upgrade() {
            host.watch_boot_dialogs(self.params.dialogs);
        }
        let mut next_look: Option<Instant> = None;
        while self.await_work(next_look.take()) {
            let (Some(host), Some(inboxes)) = (self.host.upgrade(), self.inboxes.upgrade()) else {
                break;
            };
            if held.is_none() && inboxes.queued(&self.agent) == 0 {
                continue;
            }
            let boot = (!booted).then(|| Boot {
                settle: self.params.settle,
                mark: self.params.boot_mark,
                answer_dialogs: !answered && host.answers_boot_dialogs(),
            });
            let decision = gate(
                &host.input_state(),
                Instant::now(),
                &self.policy,
                boot,
                &mut unready_since,
            );
            self.observe(&decision);
            // Tell the launcher what the first message waits on, where it is a boot dialog marion
            // does not answer: it raises the node's attention item and, past the grace, ends it.
            // `Expired` is final: the launcher ends the node on it.
            let showing = host.input_state().boot_dialog;
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
                    answered |= self.answer(&host, &inboxes, &mut held, dialog, boot);
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
        let DialogAnswer::Keys(keys) = dialog.answer else {
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
            Ok(Injected::Declined) => false,
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
