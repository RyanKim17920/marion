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
//! # Why the row's [`IdleSignal`] is not waited on
//!
//! The row carries `OutputQuiet{1500}` — the measured "the TUI is idle" signal — and this driver
//! deliberately does **not** wait for it. S31 measured Enter while busy on all four TUIs (codex,
//! opencode, copilot, claude): never dropped, never an interrupt, delivered right after the
//! in-flight model response (codex "submitted after next tool call", claude "steer in real-time",
//! copilot "ctrl+q enqueue", opencode a pending message). Writing while busy is therefore safe on
//! every measured row, and waiting for output quiet would only add latency — and never fire on a
//! TUI that repaints a clock. The match in [`PasteParams::of`] names the signal so a new variant,
//! or a row where busy input is *not* safe, has to decide here.
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
use marion_harness::spec::{IdleSignal, TurnDelivery};

use crate::inbox::{DeliveryPort, Inboxes, Message, render};
use crate::pty::{Injected, Injection, InputState, PtyHost};

/// The lane's word on `MessageDelivered`.
pub const VIA: &str = "pty:paste";

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// **One message as one bracketed paste.** Inside the brackets only text survives: every control
/// character except newline and tab is removed (an `ESC` could close the paste early — a child's
/// result is quoted here, and its `ESC[201~` would turn the rest into typing, its first newline a
/// submit), and a CR, alone or before LF, becomes the newline it means.
pub fn frame(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + PASTE_START.len() + PASTE_END.len());
    out.push_str(PASTE_START);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.push_str(PASTE_END);
    out
}

/// The row's paste, as this driver uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteParams {
    pub submit: &'static [u8],
    pub submit_delay: Duration,
}

impl PasteParams {
    /// The paste a row states, or `None` for every other strategy — only a `TerminalPaste` row gets
    /// an injector.
    pub fn of(delivery: TurnDelivery) -> Option<PasteParams> {
        let TurnDelivery::TerminalPaste {
            idle,
            submit,
            submit_delay_ms,
            note: _,
        } = delivery
        else {
            return None;
        };
        match idle {
            // Not waited on: every measured TUI queues or steers input written while it is busy,
            // so a paste need not wait for the turn to end. See the module doc.
            IdleSignal::OutputQuiet { .. } => {}
        }
        Some(PasteParams {
            submit,
            submit_delay: Duration::from_millis(u64::from(submit_delay_ms)),
        })
    }
}

/// Marion's side of when a paste may be typed — not measured per row, because it is about the
/// person at the terminal, not the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PastePolicy {
    /// How long since the operator's last key before marion types. 1.5 s: past a typing pause,
    /// short of a noticeable delivery delay.
    pub operator_quiet: Duration,
    /// How long a message waits for the node to turn bracketed paste on before it is dropped.
    /// 30 s covers every measured TUI's boot; past it the node is not a terminal marion can paste
    /// into safely, and saying so beats holding the message for the node's whole life.
    pub paste_mode_grace: Duration,
    /// How often a held message looks again. The holds end on state marion is not told about (a
    /// key, a mode), so they are re-read — only while a message waits.
    pub recheck: Duration,
}

impl PastePolicy {
    pub const PRODUCTION: PastePolicy = PastePolicy {
        operator_quiet: Duration::from_millis(1500),
        paste_mode_grace: Duration::from_secs(30),
        recheck: Duration::from_millis(100),
    };
}

/// Why a paste is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    OperatorTyping,
    ComposerNotEmpty,
    NoBracketedPaste,
}

/// One look at the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Ready,
    /// Not now; look again at the instant.
    Wait(Hold, Instant),
    /// Never, for this message.
    Refuse(String),
}

/// **The decision**, on values: `state` as of `now`. `paste_mode_off_since` is the grace's clock —
/// set the first time bracketed paste was the only thing in the way, cleared when it comes on or
/// a message is refused for it.
pub fn gate(
    state: &InputState,
    now: Instant,
    policy: &PastePolicy,
    paste_mode_off_since: &mut Option<Instant>,
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
    if state.bracketed_paste {
        *paste_mode_off_since = None;
        return Gate::Ready;
    }
    let since = *paste_mode_off_since.get_or_insert(now);
    if now.saturating_duration_since(since) >= policy.paste_mode_grace {
        *paste_mode_off_since = None;
        return Gate::Refuse(format!(
            "the node's terminal has not enabled bracketed paste (DECSET 2004) for {} s, and \
             marion never types a message unbracketed: a newline in it would submit part of it",
            policy.paste_mode_grace.as_secs()
        ));
    }
    Gate::Wait(Hold::NoBracketedPaste, now + policy.recheck)
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
    #[cfg_attr(not(test), expect(dead_code, reason = "joined only by tests"))]
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
    ) -> Option<Arc<PasteInjector>> {
        Self::spawn(agent, host, inboxes, params, PastePolicy::PRODUCTION, None)
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
        let mut paste_mode_off_since = None;
        let mut next_look: Option<Instant> = None;
        while self.await_work(next_look.take()) {
            let (Some(host), Some(inboxes)) = (self.host.upgrade(), self.inboxes.upgrade()) else {
                break;
            };
            if held.is_none() && inboxes.queued(&self.agent) == 0 {
                continue;
            }
            let decision = gate(
                &host.input_state(),
                Instant::now(),
                &self.policy,
                &mut paste_mode_off_since,
            );
            self.observe(&decision);
            next_look = Some(match decision {
                Gate::Wait(_, at) => at,
                Gate::Refuse(reason) => {
                    if let Some(m) = held.take().or_else(|| inboxes.take_next(&self.agent)) {
                        inboxes.dropped(&self.agent, &m.id, &reason);
                    }
                    // The next message, if any, is looked at now.
                    Instant::now()
                }
                Gate::Ready => match held.take().or_else(|| inboxes.take_next(&self.agent)) {
                    Some(m) => {
                        held = self.paste(&host, &inboxes, m);
                        if held.is_some() {
                            Instant::now() + self.policy.recheck
                        } else {
                            Instant::now()
                        }
                    }
                    None => Instant::now(),
                },
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

    /// Paste `m`, or hand it back when the operator got to the terminal first.
    fn paste(&self, host: &PtyHost, inboxes: &Inboxes, m: Message) -> Option<Message> {
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
        let admit = |state: &InputState| {
            gate(state, Instant::now(), &self.policy, &mut None) == Gate::Ready
        };
        match host.inject(&injection, &admit) {
            Ok(Injected::Written) => {
                inboxes.delivered(&self.agent, &m.id, VIA);
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
mod tests;
