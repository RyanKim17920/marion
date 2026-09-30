//! Transparent client relay for a claimed native facade.

#![cfg(all(
    target_has_atomic = "32",
    any(
        all(
            target_os = "macos",
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        all(
            target_os = "linux",
            any(target_arch = "aarch64", target_arch = "x86_64")
        )
    )
))]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

use marion_core::contract::AgentId;
use marion_core::proto::{
    Call, ClientNotification, Event, Frame, Input, MethodResult, NodePaneReadyV1, NodePaneWriteV1,
    RequestId,
};
use marion_tui::{Action, Keys};

mod sequence;

type Refusal = String;

/// How long one write to the supervisor may block. A bound on a stalled peer, never a cadence.
const WRITE_BOUND: std::time::Duration = std::time::Duration::from_millis(50);
/// The attach is request 1; the status row's `tree/subscribe` is the relay's only other request.
const STATUS_SUBSCRIBE_ID: RequestId = RequestId::Number(2);
static RESIZED: AtomicBool = AtomicBool::new(false);
/// "Look at the flags again": rung by the relay's signal handler after it publishes a resize or a
/// termination, so the pump's `poll(2)` returns for them. Process-wide because the handler is; what
/// one session's keyboard worker says goes to that session's own [`RawPaneSession::nudge`]. [`crate::wake::Pipe::wake`] is an atomic swap and one `write(2)`, both
/// async-signal-safe; the pipe exists before any handler is installed ([`relay_wake`] runs in
/// [`RelaySignalGuard::acquire`] first). `None` inside only if no descriptor could be had, and the
/// pump then re-checks at [`crate::wake::DEGRADED_RECHECK`].
static RELAY_WAKE: std::sync::OnceLock<Option<crate::wake::Pipe>> = std::sync::OnceLock::new();

fn relay_wake() -> Option<&'static crate::wake::Pipe> {
    RELAY_WAKE
        .get_or_init(|| crate::wake::Pipe::new().ok())
        .as_ref()
}

fn ring_relay_wake() {
    if let Some(Some(wake)) = RELAY_WAKE.get() {
        wake.wake();
    }
}
static RELAY_SIGNAL_EVENTS: AtomicU32 = AtomicU32::new(0);
static FIRST_RELAY_SIGNAL: AtomicI32 = AtomicI32::new(0);
static RELAY_SIGNAL_OWNER: Mutex<()> = Mutex::new(());
static RELAY_SIGNAL_OWNERSHIP_POISONED: AtomicBool = AtomicBool::new(false);
/// True while the current owner keeps an eligible default `SIGTSTP` blocked on the relay thread.
static STOP_OWNED: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static BEFORE_REDELIVERY_WHILE_OWNER_HELD: Mutex<Option<Box<dyn FnOnce() + Send>>> =
    Mutex::new(None);
#[cfg(test)]
static AFTER_RESIZE_SIGNAL_ACQUIRE: Mutex<Option<Box<dyn FnOnce() + Send>>> = Mutex::new(None);
#[cfg(test)]
static FAIL_RELAY_SIGNAL_INSTALL_AT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static RELAY_SIGNAL_INSTALL_ATTEMPT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static FAIL_RELAY_SIGNAL_RESTORE_ATTEMPTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static RELAY_SIGNAL_RESTORE_ATTEMPT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static AFTER_RELAY_SIGNAL_RESTORE: Mutex<Option<Box<dyn FnOnce() + Send>>> = Mutex::new(None);
#[cfg(test)]
thread_local! {
    /// Fires on the keyboard worker's own thread once a keystroke's bytes have gone out, so a test
    /// can hold the worker in exactly that window and pin what the pump concludes from an EOF
    /// arriving inside it. Thread-local because the relay's other fixtures run keyboard workers of
    /// their own in this process, and a process-global slot would be taken by whichever wrote
    /// first; the fixture arms it from its own `read`, which already runs on that thread.
    static AFTER_KEYBOARD_INPUT_WRITE: std::cell::RefCell<Option<Box<dyn FnOnce() + Send>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn after_keyboard_input_write() {
    if let Some(hook) = AFTER_KEYBOARD_INPUT_WRITE.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}
/// Fires inside `suspend_until_continued` at the stop, while termination handlers are blocked, so
/// a test can make a termination pending during the stop.
#[cfg(test)]
static WHILE_STOPPED_WITH_TERMINATIONS_BLOCKED: Mutex<Option<Box<dyn FnOnce() + Send>>> =
    Mutex::new(None);
unsafe extern "C" {
    #[cfg(test)]
    fn signal(sig: std::ffi::c_int, handler: usize) -> usize;
    fn sigaction(sig: std::ffi::c_int, action: *const Sigaction, prior: *mut Sigaction) -> i32;
    fn pthread_sigmask(how: std::ffi::c_int, set: *const SigSet, old: *mut SigSet) -> i32;
    fn sigpending(set: *mut SigSet) -> i32;
    fn raise(signal: std::ffi::c_int) -> std::ffi::c_int;
}

#[cfg(target_os = "macos")]
const SIG_BLOCK: std::ffi::c_int = 1;
#[cfg(target_os = "linux")]
const SIG_BLOCK: std::ffi::c_int = 0;
#[cfg(target_os = "macos")]
const SIG_UNBLOCK: std::ffi::c_int = 2;
#[cfg(target_os = "linux")]
const SIG_UNBLOCK: std::ffi::c_int = 1;
const SIG_DFL: usize = 0;
const SIG_IGN: usize = 1;

const SIGWINCH: std::ffi::c_int = 28;
const SIGHUP: std::ffi::c_int = 1;
const SIGINT: std::ffi::c_int = 2;
const SIGTERM: std::ffi::c_int = 15;
#[cfg(target_os = "macos")]
const SIGTSTP: std::ffi::c_int = 18;
#[cfg(target_os = "linux")]
const SIGTSTP: std::ffi::c_int = 20;
/// Signals Marion replaces with its own atomic handler while a relay owns the process. `SIGTSTP`
/// is deliberately absent: an eligible stop is owned by keeping it blocked and observing it
/// pending, so the terminal's original default stop is revealed rather than re-implemented.
const HANDLED_SIGNALS: [std::ffi::c_int; 4] = [SIGWINCH, SIGINT, SIGTERM, SIGHUP];
/// The handled signals whose first arrival ends the relay and is redelivered after restoration.
const TERMINATION_SIGNALS: [std::ffi::c_int; 3] = [SIGINT, SIGTERM, SIGHUP];

const INTERRUPTED: u32 = 1 << 0;
const TERMINATED: u32 = 1 << 1;
const HUNG_UP: u32 = 1 << 2;

#[cfg(target_os = "macos")]
type SigSet = u32;
#[cfg(target_os = "macos")]
const fn empty_sigset() -> SigSet {
    0
}

#[cfg(target_os = "linux")]
type SigSet = [usize; 128 / std::mem::size_of::<usize>()];
#[cfg(target_os = "linux")]
const fn empty_sigset() -> SigSet {
    [0; 128 / std::mem::size_of::<usize>()]
}

fn signal_in_set(set: &SigSet, signal: std::ffi::c_int) -> bool {
    #[cfg(target_os = "macos")]
    {
        *set & (1 << (signal - 1)) != 0
    }
    #[cfg(target_os = "linux")]
    {
        let bit = usize::try_from(signal - 1).expect("relay signals are positive");
        set[bit / usize::BITS as usize] & (1usize << (bit % usize::BITS as usize)) != 0
    }
}

fn signal_set(signal: std::ffi::c_int) -> SigSet {
    #[cfg(target_os = "macos")]
    {
        1 << (signal - 1)
    }
    #[cfg(target_os = "linux")]
    {
        let mut set = empty_sigset();
        let bit = usize::try_from(signal - 1).expect("relay signals are positive");
        set[bit / usize::BITS as usize] |= 1usize << (bit % usize::BITS as usize);
        set
    }
}

fn current_thread_signal_mask() -> std::io::Result<SigSet> {
    let mut mask = empty_sigset();
    // SAFETY: a null replacement only queries the calling thread's mask into a valid out pointer.
    let error = unsafe { pthread_sigmask(SIG_BLOCK, std::ptr::null(), &mut mask) };
    if error == 0 {
        Ok(mask)
    } else {
        // `pthread_sigmask` returns the errno value directly instead of setting thread-local errno.
        Err(std::io::Error::from_raw_os_error(error))
    }
}

/// Block or unblock one signal on the calling thread only.
fn change_thread_signal_mask(how: std::ffi::c_int, signal: std::ffi::c_int) -> std::io::Result<()> {
    let set = signal_set(signal);
    // SAFETY: `set` is a valid platform `sigset_t`; a null old-mask pointer is permitted.
    let error = unsafe { pthread_sigmask(how, &set, std::ptr::null_mut()) };
    if error == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(error))
    }
}

fn pending_signals() -> std::io::Result<SigSet> {
    let mut pending = empty_sigset();
    // SAFETY: `sigpending` writes the union of thread- and process-pending signals into a valid
    // out pointer and never consumes any of them.
    if unsafe { sigpending(&mut pending) } == 0 {
        Ok(pending)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct Sigaction {
    handler: usize,
    mask: SigSet,
    flags: i32,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct Sigaction {
    handler: usize,
    mask: SigSet,
    flags: i32,
    restorer: usize,
}

impl Sigaction {
    const fn zeroed() -> Self {
        Self {
            handler: 0,
            mask: empty_sigset(),
            flags: 0,
            #[cfg(target_os = "linux")]
            restorer: 0,
        }
    }

    fn relay_handler() -> Self {
        Self {
            handler: on_relay_signal as *const () as usize,
            mask: empty_sigset(),
            // The relay retries interrupted reads itself. Zero avoids importing a platform-specific
            // SA_RESTART value (Darwin and Linux deliberately assign different bits).
            flags: 0,
            #[cfg(target_os = "linux")]
            restorer: 0,
        }
    }
}

extern "C" fn on_relay_signal(signal: std::ffi::c_int) {
    match signal {
        SIGWINCH => RESIZED.store(true, Ordering::SeqCst),
        SIGINT => {
            let _ =
                FIRST_RELAY_SIGNAL.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
            RELAY_SIGNAL_EVENTS.fetch_or(INTERRUPTED, Ordering::SeqCst);
        }
        SIGTERM => {
            let _ =
                FIRST_RELAY_SIGNAL.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
            RELAY_SIGNAL_EVENTS.fetch_or(TERMINATED, Ordering::SeqCst);
        }
        SIGHUP => {
            let _ =
                FIRST_RELAY_SIGNAL.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
            RELAY_SIGNAL_EVENTS.fetch_or(HUNG_UP, Ordering::SeqCst);
        }
        _ => {}
    }
    ring_relay_wake();
}

struct PriorSignalAction {
    signal: std::ffi::c_int,
    action: Sigaction,
}

/// Undo a part-built acquisition and report the failure that forced it.
///
/// A rollback that itself fails poisons process-global ownership rather than being swallowed: no
/// later relay could then know which dispositions the process is actually left holding, and
/// `rolling_back` names which rollback it was so the operator reads one sentence, not two.
fn roll_back_acquisition(
    prior: &mut Vec<PriorSignalAction>,
    failure: String,
    rolling_back: &str,
) -> Refusal {
    match restore_signal_actions_matching(prior, |_| true) {
        Ok(()) => {
            clear_relay_signal_state();
            failure
        }
        Err(rollback) => {
            poison_relay_signal_ownership();
            format!("{failure}; {rolling_back}: {rollback}")
        }
    }
}

/// Exclusive ownership of marion's process-global relay handlers for one native relay session.
pub(crate) struct RelaySignalGuard {
    /// Actions Marion's handler still replaces; each is removed the moment it is restored.
    prior: Vec<PriorSignalAction>,
    /// An eligible default `SIGTSTP` is blocked on the relay thread for the life of the guard.
    stop_owned: bool,
    captured_signal: Option<std::ffi::c_int>,
    _owner: MutexGuard<'static, ()>,
}

#[derive(Debug)]
struct RelaySignalRestoreError {
    signal: std::ffi::c_int,
    source: std::io::Error,
}

impl std::fmt::Display for RelaySignalRestoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "restoring process signal action {} failed: {}",
            self.signal, self.source
        )
    }
}

impl std::error::Error for RelaySignalRestoreError {}

impl RelaySignalGuard {
    pub(crate) fn acquire() -> Result<Self, Refusal> {
        let (owner, stop_owned) = Self::claim_ownership()?;
        relay_wake();
        // No signal edge from a prior owner may leak into this session.
        RESIZED.store(false, Ordering::SeqCst);
        RELAY_SIGNAL_EVENTS.store(0, Ordering::SeqCst);
        FIRST_RELAY_SIGNAL.store(0, Ordering::SeqCst);
        #[cfg(test)]
        RELAY_SIGNAL_INSTALL_ATTEMPT.store(0, Ordering::SeqCst);
        let mut prior = Self::install_relay_handlers()?;
        if stop_owned && let Err(error) = change_thread_signal_mask(SIG_BLOCK, SIGTSTP) {
            return Err(roll_back_acquisition(
                &mut prior,
                format!("blocking SIGTSTP on the native relay thread: {error}"),
                "rolling back process signal actions failed",
            ));
        }
        STOP_OWNED.store(stop_owned, Ordering::SeqCst);
        Ok(Self {
            prior,
            stop_owned,
            captured_signal: None,
            _owner: owner,
        })
    }

    /// The exclusive claim on marion's process-global relay handlers, and whether this relay also
    /// owns the terminal's default stop.
    ///
    /// Nothing process-global is touched here. A thread that already blocks one of the signals it
    /// is about to handle could never receive it, so it is refused before it can clear an edge or
    /// displace another relay's disposition.
    fn claim_ownership() -> Result<(MutexGuard<'static, ()>, bool), Refusal> {
        let owner = match RELAY_SIGNAL_OWNER.try_lock() {
            Ok(owner) => owner,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err("another native relay already owns SIGWINCH and relay signals".into());
            }
        };
        if RELAY_SIGNAL_OWNERSHIP_POISONED.load(Ordering::SeqCst) {
            return Err(
                "process-global native relay signal ownership is poisoned after failed restoration"
                    .into(),
            );
        }
        let mask = current_thread_signal_mask()
            .map_err(|error| format!("querying the native relay thread signal mask: {error}"))?;
        let blocked: Vec<_> = HANDLED_SIGNALS
            .iter()
            .copied()
            .filter(|signal| signal_in_set(&mask, *signal))
            .collect();
        if !blocked.is_empty() {
            return Err(format!(
                "native relay thread blocked required signals {blocked:?}"
            ));
        }
        let stop_owned = stop_eligible(&mask)?;
        Ok((owner, stop_owned))
    }

    /// Marion's handler on every relay signal, with each displaced action kept so the guard can
    /// put it back. A failure part-way through leaves the process as it was found.
    fn install_relay_handlers() -> Result<Vec<PriorSignalAction>, Refusal> {
        let action = Sigaction::relay_handler();
        let mut prior = Vec::with_capacity(HANDLED_SIGNALS.len());
        for signal in HANDLED_SIGNALS {
            let mut prior_action = Sigaction::zeroed();
            if let Err(error) = install_relay_signal_action(signal, &action, &mut prior_action) {
                return Err(roll_back_acquisition(
                    &mut prior,
                    format!("installing process signal action {signal}: {error}"),
                    "rolling back partially installed process signal actions failed",
                ));
            }
            prior.push(PriorSignalAction {
                signal,
                action: prior_action,
            });
        }
        Ok(prior)
    }

    /// Whether this relay owns the terminal's default stop: `SIGTSTP` was default and unblocked
    /// at acquisition, so it now stays blocked and is only ever observed pending.
    #[cfg(test)]
    pub(crate) fn owns_stop(&self) -> bool {
        self.stop_owned
    }

    /// The cutoff before a stop: block Marion's termination handlers on the relay thread. Once the
    /// keyboard worker is joined, no Marion handler can run until `unblock_terminations`.
    fn block_terminations(&self) -> Result<(), Refusal> {
        change_termination_mask(SIG_BLOCK)
    }

    fn unblock_terminations(&self) -> Result<(), Refusal> {
        change_termination_mask(SIG_UNBLOCK)
    }

    /// Reveal the owned stop to its untouched default action. Returns only after `SIGCONT`, or at
    /// once if `SIGCONT` already cancelled the pending stop; Marion never synthesizes one.
    fn reveal_stop(&self) -> Result<(), Refusal> {
        change_thread_signal_mask(SIG_UNBLOCK, SIGTSTP)
            .map_err(|error| format!("revealing the pending SIGTSTP: {error}"))
    }

    fn reblock_stop(&self) -> Result<(), Refusal> {
        change_thread_signal_mask(SIG_BLOCK, SIGTSTP)
            .map_err(|error| format!("reblocking SIGTSTP after continue: {error}"))
    }

    fn restore_result(&mut self) -> Result<Option<std::ffi::c_int>, RelaySignalRestoreError> {
        if self.prior.is_empty() && !self.stop_owned {
            return Ok(None);
        }
        if let Err(error) = restore_signal_actions_matching(&mut self.prior, is_termination_signal)
        {
            let _ = restore_signal_actions_matching(&mut self.prior, |signal| {
                !is_termination_signal(signal)
            });
            poison_relay_signal_ownership();
            return Err(error);
        }
        // All prior termination dispositions are live before this exchange. A signal handled
        // before its restoration is represented here; one arriving after restoration invokes its
        // prior disposition directly and cannot be erased by Marion.
        let captured_signal = match FIRST_RELAY_SIGNAL.swap(0, Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        };
        self.captured_signal = self.captured_signal.or(captured_signal);
        if let Err(error) = restore_signal_actions_matching(&mut self.prior, |signal| {
            !is_termination_signal(signal)
        }) {
            poison_relay_signal_ownership();
            return Err(error);
        }
        // The exact prior mask had SIGTSTP unblocked. A stop still pending now takes effect under
        // its untouched default action, exactly as it would have without Marion.
        if self.stop_owned {
            if let Err(source) = change_thread_signal_mask(SIG_UNBLOCK, SIGTSTP) {
                poison_relay_signal_ownership();
                return Err(RelaySignalRestoreError {
                    signal: SIGTSTP,
                    source,
                });
            }
            self.stop_owned = false;
            STOP_OWNED.store(false, Ordering::SeqCst);
        }
        clear_relay_signal_state();
        Ok(self.captured_signal.take())
    }
}

/// Whether the owned default `SIGTSTP` is pending. Observation never consumes it, so a stop stays
/// pending until the relay reveals it or ownership is restored.
fn owned_stop_pending() -> std::io::Result<bool> {
    if !STOP_OWNED.load(Ordering::SeqCst) {
        return Ok(false);
    }
    pending_signals().map(|pending| signal_in_set(&pending, SIGTSTP))
}

fn change_termination_mask(how: std::ffi::c_int) -> Result<(), Refusal> {
    for signal in TERMINATION_SIGNALS {
        change_thread_signal_mask(how, signal).map_err(|error| {
            format!("changing the relay thread mask for termination signal {signal}: {error}")
        })?;
    }
    Ok(())
}

/// Decide `SIGTSTP` ownership from its current disposition, mutating nothing.
///
/// Default and unblocked: owned by blocking, so the original stop can be revealed later. Ignored
/// or already blocked: excluded and left untouched. Custom: refused, because Marion neither
/// chains nor replays foreign stop handlers.
fn stop_eligible(mask: &SigSet) -> Result<bool, Refusal> {
    let mut current = Sigaction::zeroed();
    // SAFETY: a null replacement only queries the current action into a valid out pointer.
    if unsafe { sigaction(SIGTSTP, std::ptr::null(), &mut current) } != 0 {
        return Err(format!(
            "querying the SIGTSTP disposition: {}",
            std::io::Error::last_os_error()
        ));
    }
    match current.handler {
        SIG_DFL => Ok(!signal_in_set(mask, SIGTSTP)),
        SIG_IGN => Ok(false),
        _ => Err(
            "a custom SIGTSTP action is installed; the native relay reveals only the default stop"
                .into(),
        ),
    }
}

fn is_termination_signal(signal: std::ffi::c_int) -> bool {
    TERMINATION_SIGNALS.contains(&signal)
}

fn install_relay_signal_action(
    signal: std::ffi::c_int,
    action: &Sigaction,
    prior: &mut Sigaction,
) -> std::io::Result<()> {
    #[cfg(test)]
    {
        let attempt = RELAY_SIGNAL_INSTALL_ATTEMPT.fetch_add(1, Ordering::SeqCst) + 1;
        if FAIL_RELAY_SIGNAL_INSTALL_AT
            .compare_exchange(attempt, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Err(std::io::Error::other(
                "injected signal action installation failure",
            ));
        }
    }
    // SAFETY: both values exactly match libc's supported Darwin/Linux `struct sigaction` layout.
    // This one call atomically installs marion's action and captures every field of the prior
    // action, leaving no query/install clobber window for this signal.
    if unsafe { sigaction(signal, action, prior) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
fn fail_relay_signal_install_at(attempt: usize) {
    assert!((1..=HANDLED_SIGNALS.len()).contains(&attempt));
    RELAY_SIGNAL_INSTALL_ATTEMPT.store(0, Ordering::SeqCst);
    FAIL_RELAY_SIGNAL_INSTALL_AT.store(attempt, Ordering::SeqCst);
}

#[cfg(test)]
fn fail_relay_signal_restore_at(attempt: usize) {
    fail_relay_signal_restore_at_attempts(&[attempt]);
}

#[cfg(test)]
fn fail_relay_signal_restore_at_attempts(attempts: &[usize]) {
    let mut mask = 0;
    for &attempt in attempts {
        assert!((1..=HANDLED_SIGNALS.len()).contains(&attempt));
        mask |= 1 << (attempt - 1);
    }
    RELAY_SIGNAL_RESTORE_ATTEMPT.store(0, Ordering::SeqCst);
    FAIL_RELAY_SIGNAL_RESTORE_ATTEMPTS.store(mask, Ordering::SeqCst);
}

fn restore_signal_action(prior: &PriorSignalAction) -> std::io::Result<()> {
    #[cfg(test)]
    {
        let attempt = RELAY_SIGNAL_RESTORE_ATTEMPT.fetch_add(1, Ordering::SeqCst) + 1;
        let attempt_mask = 1 << (attempt - 1);
        if FAIL_RELAY_SIGNAL_RESTORE_ATTEMPTS.fetch_and(!attempt_mask, Ordering::SeqCst)
            & attempt_mask
            != 0
        {
            return Err(std::io::Error::other(
                "injected signal action restoration failure",
            ));
        }
    }
    // SAFETY: this action is the unmodified value libc returned for this exact signal.
    if unsafe { sigaction(prior.signal, &prior.action, std::ptr::null_mut()) } == 0 {
        #[cfg(test)]
        if let Some(hook) = AFTER_RELAY_SIGNAL_RESTORE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            hook();
        }
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Restore the matching actions in reverse installation order, removing each one restored so a
/// retry sees only what is still Marion's. Every match is attempted; the first failure is reported.
fn restore_signal_actions_matching(
    prior: &mut Vec<PriorSignalAction>,
    matches: impl Fn(std::ffi::c_int) -> bool,
) -> Result<(), RelaySignalRestoreError> {
    let mut first_error = None;
    let mut index = prior.len();
    while index > 0 {
        index -= 1;
        if !matches(prior[index].signal) {
            continue;
        }
        match restore_signal_action(&prior[index]) {
            Ok(()) => {
                prior.remove(index);
            }
            Err(source) => {
                first_error.get_or_insert(RelaySignalRestoreError {
                    signal: prior[index].signal,
                    source,
                });
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn clear_relay_signal_state() {
    RESIZED.store(false, Ordering::SeqCst);
    RELAY_SIGNAL_EVENTS.store(0, Ordering::SeqCst);
    FIRST_RELAY_SIGNAL.store(0, Ordering::SeqCst);
}

/// No later relay in this process may install handlers over actions Marion could not restore.
fn poison_relay_signal_ownership() {
    RELAY_SIGNAL_OWNERSHIP_POISONED.store(true, Ordering::SeqCst);
}

impl Drop for RelaySignalGuard {
    fn drop(&mut self) {
        // Restore in reverse installation order while `_owner` still excludes a later relay. A
        // failed restoration has already poisoned ownership and left `FIRST_RELAY_SIGNAL` alone,
        // since a still-installed Marion handler could publish into it; the termination captured
        // so far is redelivered either way.
        let redeliver = match self.restore_result() {
            Ok(captured_signal) => captured_signal,
            Err(_) => self.captured_signal,
        };
        if let Some(signal) = redeliver {
            let _ = redeliver_signal(signal);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelayStop {
    Complete,
    ExternalSignal(std::ffi::c_int),
    /// The owned default `SIGTSTP` is pending; the terminal must be restored before it is revealed.
    Suspend,
}

struct OwnedFdReader(std::os::fd::OwnedFd);

impl Read for OwnedFdReader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        rustix::io::read(&self.0, bytes).map_err(Into::into)
    }
}

struct OwnedFdWriter(std::os::fd::OwnedFd);

/// How long the terminal's output queue may stay full before the relay calls it a stall. A pty
/// drained by any live terminal empties in microseconds; one that stays full this long has no
/// reader, and the relay ends rather than holding a node's output hostage for ever.
const OUTPUT_STALL: std::time::Duration = std::time::Duration::from_secs(10);

// `poll(2)`, declared by hand the way `marion_tui::guard` declares it: the crate's `rustix` has
// no event feature, and one descriptor's writability is all the relay ever asks.
unsafe extern "C" {
    fn poll(fds: *mut PollFd, nfds: NfdsT, timeout_ms: std::ffi::c_int) -> std::ffi::c_int;
}

/// `nfds_t`: `unsigned long` on Linux, `unsigned int` everywhere else marion runs.
#[cfg(target_os = "linux")]
type NfdsT = u64;
#[cfg(not(target_os = "linux"))]
type NfdsT = u32;

#[repr(C)]
struct PollFd {
    fd: std::ffi::c_int,
    events: i16,
    revents: i16,
}

/// `POLLOUT` and `POLLNVAL` are `0x0004` and `0x0020` on Darwin and Linux alike.
const POLLOUT: i16 = 0x0004;
const POLLNVAL: i16 = 0x0020;

/// Block until `fd` will take more bytes, or `deadline` passes. `Ok(true)` is writable;
/// `Ok(false)` is the deadline; a closed descriptor is an error.
fn wait_writable(
    fd: std::os::fd::BorrowedFd<'_>,
    deadline: std::time::Instant,
) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        let mut pfd = PollFd {
            fd: fd.as_raw_fd(),
            events: POLLOUT,
            revents: 0,
        };
        let timeout_ms =
            std::ffi::c_int::try_from(remaining.as_millis().max(1)).unwrap_or(std::ffi::c_int::MAX);
        // SAFETY: `pfd` is a live, correctly laid out `struct pollfd` for the duration of the
        // call, and `fd` is a live borrowed descriptor.
        let ready = unsafe { poll(&mut pfd, 1, timeout_ms) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            return Ok(false);
        }
        if pfd.revents & POLLNVAL != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the terminal output descriptor was closed under the relay",
            ));
        }
        // `POLLOUT`, `POLLHUP` or `POLLERR`: the write decides, and cannot block after any.
        return Ok(true);
    }
}

/// One write to the operator's terminal. **`WouldBlock` is a full queue, not a failure.** The
/// descriptor is nonblocking while the relay runs, and a TUI paints its frame in one burst larger
/// than a pty's output queue; a writer that reported the first refusal would end the relay
/// mid-frame whenever the operator's terminal drained a little slower than the harness painted
/// (`native_facade_e2e.rs` caught exactly that: the client gone, the frame's tail never written),
/// and would fail the finish stage's passive cleanup bytes whenever a detach landed under a frame
/// still draining (the same file's copilot lane: a clean `^]d` exited 1 with `EAGAIN`). A refused
/// write blocks on the descriptor's writability, not on time, and only a queue nobody drains within
/// [`OUTPUT_STALL`] becomes an error, named as a stall.
fn write_waiting_out_a_full_queue(
    fd: std::os::fd::BorrowedFd<'_>,
    bytes: &[u8],
) -> std::io::Result<usize> {
    let deadline = std::time::Instant::now() + OUTPUT_STALL;
    loop {
        match rustix::io::write(fd, bytes) {
            Ok(written) => return Ok(written),
            // `EWOULDBLOCK` is `EAGAIN` on both supported targets.
            Err(rustix::io::Errno::AGAIN) => {
                if !wait_writable(fd, deadline)? {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "the terminal's output queue stayed full for {OUTPUT_STALL:?}; \
                             nothing is reading the operator's terminal"
                        ),
                    ));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

impl Write for OwnedFdWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        use std::os::fd::AsFd;
        write_waiting_out_a_full_queue(self.0.as_fd(), bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Claim the authenticated launch on its issuing socket and transparently relay its pane until
/// End or operator detach. The terminal guard remains live across every protocol and I/O failure.
pub(crate) fn run(
    handoff: crate::native_bootstrap::NativeFacadeHandoff,
) -> Result<RelayExit, Refusal> {
    let claimed = handoff
        .claim()
        .map_err(|error| format!("claiming the native launch: {error}"))?;
    let (agent_id, tty, stream) = claimed.into_parts();
    let (mut terminal, signals) = setup_after_signal_acquire(|| {
        tty.enter_native_relay()
            .map_err(|error| format!("entering native terminal relay mode: {error}"))
    })?;
    relay_claimed(agent_id, &mut terminal, stream, signals)
}

fn setup_after_signal_acquire<T>(
    setup: impl FnOnce() -> Result<T, Refusal>,
) -> Result<(T, RelaySignalGuard), Refusal> {
    let signals = RelaySignalGuard::acquire()?;
    let value = setup()?;
    Ok((value, signals))
}

/// Relay the claimed pane over `stream` until End, detach, failure, or an owned termination
/// signal, then restore the terminal and the prior signal actions in one finish stage.
pub(crate) fn relay_claimed(
    agent_id: AgentId,
    terminal: &mut crate::native_tty::NativeRelayTerminal,
    stream: UnixStream,
    signals: RelaySignalGuard,
) -> Result<RelayExit, Refusal> {
    let node = agent_id.clone();
    let mut detached = false;
    let primary = (|| {
        let input_fd = {
            use std::os::fd::AsRawFd;
            terminal.stdin().as_raw_fd()
        };
        let stdout = rustix::io::fcntl_dupfd_cloexec(terminal.stdout(), 3)
            .map_err(|error| format!("retaining native terminal output: {error}"))?;
        let mut session = RawPaneSession::open_with_io(
            stream,
            agent_id,
            retained_terminal_input(terminal)?,
            Some(input_fd),
            OwnedFdWriter(stdout),
            None,
        )?;
        #[cfg(test)]
        if std::env::var_os("MARION_NATIVE_RELAY_PROBE").is_some() {
            eprintln!("NATIVE_RELAY_READY");
        }
        let result = loop {
            match session.pump() {
                Ok(RelayStop::Suspend) => {
                    suspend_until_continued(terminal, &mut session, &signals)?
                }
                other => break other,
            }
        };
        // The operator's shell gets its whole screen back: no scroll region above a row marion
        // no longer paints. A primary failure outranks a failure to say so.
        let released = session.release_status();
        detached = session.detached.load(Ordering::SeqCst);
        drop(session);
        result.and_then(|stop| released.map(|()| stop))
    })();
    finish_claimed_relay(terminal, primary, signals).map(|()| {
        if detached {
            RelayExit::Detached(node)
        } else {
            RelayExit::Ended
        }
    })
}

/// How a relay that did not fail ended, for the one line the facade prints after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RelayExit {
    /// The node's End, a signal, or anything else that was not the operator leaving.
    Ended,
    /// The operator typed `^] d`; the node keeps running.
    Detached(AgentId),
}

fn retained_terminal_input(
    terminal: &crate::native_tty::NativeRelayTerminal,
) -> Result<OwnedFdReader, Refusal> {
    rustix::io::fcntl_dupfd_cloexec(terminal.stdin(), 3)
        .map(OwnedFdReader)
        .map_err(|error| format!("retaining native terminal input: {error}"))
}

/// Stop under the terminal's own default `SIGTSTP` with the operator terminal restored, then
/// resume the relay after `SIGCONT`.
///
/// The order follows the synchronous-signal design: block Marion's termination handlers (the
/// cutoff), join the keyboard worker, write the passive cleanup bytes, restore termios and
/// descriptor flags, let a termination that beat the cutoff win, and only then reveal the still
/// pending stop to its untouched default action. After `SIGCONT` the stop is reblocked and the
/// termination handlers exposed again before raw mode is re-entered, the keyboard worker
/// restarted, and geometry reconciliation forced. Marion never raises or forwards the stop, and
/// the supervisor's node child is not signalled: it keeps running behind the relay.
fn suspend_until_continued<W: Write>(
    terminal: &mut crate::native_tty::NativeRelayTerminal,
    session: &mut RawPaneSession<W>,
    signals: &RelaySignalGuard,
) -> Result<(), Refusal> {
    signals.block_terminations()?;
    if !session.park_keyboard() {
        // The operator detached or the worker failed before the stop; the pump reports that.
        return signals.unblock_terminations();
    }
    // The shell in front during the stop gets the whole screen; the row comes back on resume,
    // because it is still wanted.
    let passive = session
        .release_status()
        .and_then(|()| write_passive_terminal_cleanup(terminal).map_err(|error| error.to_string()))
        .map_err(|error| {
            format!(
                "native relay cleanup stage `passive terminal bytes` failed before the stop: \
                 {error}"
            )
        });
    let restored = terminal
        .restore_for_suspend()
        .map_err(|error| format!("restoring the native terminal before the stop: {error}"));
    if let Err(error) = passive.and(restored) {
        let _ = signals.unblock_terminations();
        return Err(error);
    }
    if FIRST_RELAY_SIGNAL.load(Ordering::SeqCst) != 0 {
        // A termination reached its handler before the cutoff and dominates the stop.
        return signals.unblock_terminations();
    }
    #[cfg(test)]
    if let Some(hook) = WHILE_STOPPED_WITH_TERMINATIONS_BLOCKED
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    {
        hook();
    }
    signals.reveal_stop()?;
    // Execution continues here after SIGCONT, or at once if SIGCONT cancelled the stop first.
    signals.reblock_stop()?;
    // Exposing the termination handlers delivers any termination that arrived during the stop. A
    // late termination dominates the resume: leave the terminal restored and let the relay finish
    // through the termination path instead of re-entering raw mode.
    signals.unblock_terminations()?;
    if FIRST_RELAY_SIGNAL.load(Ordering::SeqCst) != 0 {
        return Ok(());
    }
    terminal.reenter_after_continue().map_err(|error| {
        format!("re-entering native terminal relay mode after continue: {error}")
    })?;
    session.resume_keyboard(retained_terminal_input(terminal)?)?;
    // The terminal may have been resized while stopped; a resize is never inferred from a signal.
    RESIZED.store(true, Ordering::SeqCst);
    Ok(())
}

pub(crate) fn finish_claimed_relay(
    terminal: &mut crate::native_tty::NativeRelayTerminal,
    primary: Result<RelayStop, Refusal>,
    mut signals: RelaySignalGuard,
) -> Result<(), Refusal> {
    let passive_cleanup =
        write_passive_terminal_cleanup(terminal).map_err(|error| error.to_string());
    let terminal_cleanup = terminal.restore_result().map_err(|error| error.to_string());
    let signal_cleanup = signals.restore_result().map_err(|error| error.to_string());
    let result = resolve_relay_finish(
        primary,
        passive_cleanup,
        terminal_cleanup,
        signal_cleanup,
        redeliver_signal,
    );
    // `restore_result` disarms the handlers but the guard deliberately retains exclusive signal
    // ownership until synchronous redelivery has run the restored disposition. The relay thread
    // never changes its mask, whose owned-signal invariant was checked during acquisition.
    drop(signals);
    result
}

fn resolve_relay_finish(
    primary: Result<RelayStop, Refusal>,
    passive_cleanup: Result<(), String>,
    terminal_cleanup: Result<(), String>,
    signal_cleanup: Result<Option<std::ffi::c_int>, String>,
    mut redeliver: impl FnMut(std::ffi::c_int) -> Result<(), Refusal>,
) -> Result<(), Refusal> {
    let mut errors = Vec::new();
    if let Err(primary) = primary {
        errors.push(primary);
    }
    if let Err(error) = passive_cleanup {
        errors.push(format!(
            "native relay cleanup stage `passive terminal bytes` failed: {error}"
        ));
    }
    if let Err(error) = terminal_cleanup {
        errors.push(format!(
            "native relay cleanup stage `terminal restoration` failed: {error}"
        ));
    }
    let captured_signal = match signal_cleanup {
        Ok(signal) => signal,
        Err(error) => {
            errors.push(format!(
                "native relay cleanup stage `signal restoration` failed: {error}"
            ));
            None
        }
    };
    if let Some(signal) = captured_signal {
        #[cfg(test)]
        if let Some(hook) = BEFORE_REDELIVERY_WHILE_OWNER_HELD
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            hook();
        }
        if let Err(error) = redeliver(signal) {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn write_passive_terminal_cleanup(
    terminal: &crate::native_tty::NativeRelayTerminal,
) -> std::io::Result<()> {
    write_passive_terminal_cleanup_to(BorrowedTerminalWriter(terminal.stdout()))
}

/// The operator's terminal output, borrowed for the cleanup bytes the finish stages write.
struct BorrowedTerminalWriter<'a>(std::os::fd::BorrowedFd<'a>);

impl Write for BorrowedTerminalWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        write_waiting_out_a_full_queue(self.0, bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn write_passive_terminal_cleanup_to(mut output: impl Write) -> std::io::Result<()> {
    let mut bytes = b"\x1b[?2004l".to_vec();
    bytes.extend_from_slice(&marion_tui::guard::leave_bytes());
    output.write_all(&bytes)
}

fn redeliver_signal(signal: std::ffi::c_int) -> Result<(), Refusal> {
    // SAFETY: signal restoration completed before this call, so the current process observes the
    // exact prior disposition. Acquisition proved this signal is unblocked on the relay thread,
    // so `raise` invokes a caught disposition on this thread before returning, terminates for a
    // default disposition, and returns normally only for a caught or ignored disposition.
    let result = unsafe { raise(signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "native relay cleanup stage `signal redelivery` failed for {signal}: {}",
            std::io::Error::last_os_error()
        ))
    }
}

fn write_serialized<W: Write>(
    writer: &Arc<std::sync::Mutex<W>>,
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut writer = writer.lock().unwrap_or_else(|error| error.into_inner());
    writer.write_all(bytes)?;
    writer.flush()
}

/// A pane-v1 session whose display is the caller's retained stdout descriptor itself.
///
/// Unlike [`crate::attach`], this owns no grid, alternate screen, sticky-mode mirror, or painter:
/// an Output frame is one exact byte slice written to `output`.
struct RawPaneSession<W: Write> {
    stream: UnixStream,
    writer: Arc<std::sync::Mutex<UnixStream>>,
    inbound: Vec<u8>,
    id: AgentId,
    output: W,
    next_seq: u64,
    cut: u64,
    input_fd: Option<std::os::fd::RawFd>,
    /// Whether the attach kept this claim's write half. False only for a pane that had already
    /// ended when the relay reached it: the node is finished, so this session replays what it
    /// said and never sends a resize or a keystroke at it.
    writable: bool,
    /// Raised by the keyboard worker when it stops and by [`Self::park_keyboard`]; raising it
    /// wakes the worker's `poll`, and lowering it (a resume) drains it.
    leaving: Arc<crate::wake::Flag>,
    keyboard_failure: Arc<std::sync::Mutex<Option<String>>>,
    /// Whether any keystroke has been forwarded. The supervisor refuses opaque input by closing
    /// the connection (`serve::Departure::PaneInputFailed`), so an EOF before End is a refusal
    /// only if something was sent for it to refuse.
    input_sent: Arc<AtomicBool>,
    keyboard: Option<std::thread::JoinHandle<()>>,
    /// Rung by this session's keyboard worker after a status toggle, so the pump's `poll(2)`
    /// returns to paint or clear the row. The session's own, not [`RELAY_WAKE`]: two sessions in
    /// one process (the tests run many) would otherwise drain each other's wakes. `None` only if
    /// no descriptor could be had; the pump then re-checks at the degraded bound.
    nudge: Option<Arc<crate::wake::Pipe>>,
    status: StatusOverlay,
    /// Set by the keyboard worker on the operator's `^] d`, and only then: the relay's other
    /// endings are not a detach and get no reattach hint.
    detached: Arc<AtomicBool>,
}

/// The shortest gap between two repaints of the status row that node output asked for: the node
/// may have cleared the screen or switched screens under it. A change to the row's own text
/// repaints at the pump's next pass, output since the last paint sets a deadline of this long
/// after it ([`StatusOverlay::next_due`]), and an idle node gets no repaint at all.
const STATUS_REDRAW: std::time::Duration = std::time::Duration::from_millis(250);

/// marion's one optional row on the operator's terminal, and the `tree/subscribe` behind it.
///
/// Never drawn unless the operator asked (`^] s`); while asked for, redrawn when its text changed
/// and after node output at most every [`STATUS_REDRAW`]; on toggle-off, cleared once and then
/// left alone. The
/// subscription outlives the toggle: it is made once, at the first request, and kept for the
/// relay's lifetime so a later toggle-on has a current snapshot at once.
struct StatusOverlay {
    /// Flipped by the keyboard worker on each `^] s`; the pump reads it on its tick.
    wanted: Arc<AtomicBool>,
    /// The tree snapshot, once the subscribe was answered; kept current by `fold_tree_event`.
    nodes: Option<Vec<marion_core::proto::NodeSummary>>,
    /// Whether the subscribe request has been sent.
    subscribed: bool,
    /// The supervisor refused the subscribe, or it could not be sent. The row says so; the relay
    /// itself is unaffected, because a status line is never worth the node's screen.
    unavailable: bool,
    /// Whether the row, and the scroll region that stops above it, are on the terminal.
    shown: bool,
    /// Whether the node was told its window is one row shorter than the operator's, so that the
    /// last row is marion's and the node never paints it.
    reserved: bool,
    /// Something to repaint at the next tick: a tree event, the toggle, or a resize.
    dirty: bool,
    /// Node output reached the terminal since the last paint, and could have erased the row.
    touched: bool,
    last_drawn: Option<std::time::Instant>,
    rows: u16,
    cols: u16,
    /// Where the node's own byte stream stands: the row is written only at a boundary of it.
    stream: sequence::StreamPosition,
}

impl StatusOverlay {
    fn new((cols, rows): (u16, u16)) -> Self {
        Self {
            wanted: Arc::new(AtomicBool::new(false)),
            nodes: None,
            subscribed: false,
            unavailable: false,
            shown: false,
            reserved: false,
            dirty: false,
            touched: false,
            last_drawn: None,
            rows,
            cols,
            stream: sequence::StreamPosition::default(),
        }
    }

    /// The height the node is told it has: the operator's, less the row while marion holds it.
    fn node_rows(&self) -> u16 {
        if self.reserved {
            self.rows.saturating_sub(1).max(1)
        } else {
            self.rows
        }
    }

    /// Bytes to put right after each node sequence in `marks` that widened the scroll region past
    /// the node's own last row, so the node's scrolling never carries the row into its text. Only
    /// while the row is shown: otherwise the whole screen is the node's.
    fn margin_repairs(&mut self, marks: &[sequence::Mark]) -> Vec<(usize, Vec<u8>)> {
        if !self.shown || self.rows < 2 {
            return Vec::new();
        }
        let limit = self.rows - 1;
        let mut repairs = Vec::new();
        for mark in marks {
            match *mark {
                sequence::Mark::ScrollRegion { end, top, bottom } => {
                    if bottom.unwrap_or(self.rows) > limit {
                        let top = top.min(limit - 1).max(1);
                        repairs.push((end, format!("\x1b[{top};{limit}r").into_bytes()));
                    }
                }
                // RIS homes the cursor as DECSTBM does, and clears the screen the row was on.
                sequence::Mark::HardReset { end } => {
                    self.dirty = true;
                    repairs.push((end, format!("\x1b[1;{limit}r").into_bytes()));
                }
                // DECSTR leaves the cursor where it is, and has just reset the saved cursor, so a
                // save and restore around the region costs the node nothing.
                sequence::Mark::SoftReset { end } => {
                    repairs.push((end, format!("\x1b7\x1b[1;{limit}r\x1b8").into_bytes()));
                }
            }
        }
        repairs
    }

    fn resized(&mut self, cols: u16, rows: u16) {
        if (self.cols, self.rows) != (cols, rows) {
            self.cols = cols;
            self.rows = rows;
            self.dirty = true;
        }
    }

    /// Fold one subscription notification into the snapshot; a change marks the row dirty.
    fn absorb(&mut self, event: &Event) {
        if let Some(nodes) = self.nodes.as_mut()
            && crate::tree::fold_tree_event(nodes, event)
        {
            self.dirty = true;
        }
    }

    fn line(&self, id: &AgentId) -> String {
        match &self.nodes {
            _ if self.unavailable => "marion: tree unavailable".to_string(),
            None => "marion: tree pending".to_string(),
            Some(nodes) => status_line(id, nodes),
        }
    }

    /// When the pump must look at the row again with nothing else waking it: [`STATUS_REDRAW`]
    /// after the last paint, while node output has touched the terminal since and the row could be
    /// painted now. `None` otherwise — a change to the row's text arrives with a frame or a wake,
    /// and a stream stopped mid-sequence can only be finished by more of it.
    fn next_due(&self) -> Option<std::time::Instant> {
        if !self.wanted.load(Ordering::SeqCst)
            || !self.touched
            || self.rows < 2
            || !self.stream.at_boundary()
        {
            return None;
        }
        self.last_drawn.map(|drawn| drawn + STATUS_REDRAW)
    }

    /// Whether the row needs painting now: a change, or node output since a paint long enough ago.
    fn due(&self) -> bool {
        self.dirty
            || self.last_drawn.is_none()
            || (self.touched
                && self
                    .last_drawn
                    .is_some_and(|drawn| drawn.elapsed() >= STATUS_REDRAW))
    }
}

/// Why `next_frame` could not produce a frame — kept apart from the operator-facing [`Refusal`]
/// so `pump` can say what a disconnect *meant* before it becomes a message.
enum FrameError {
    /// The socket closed before End, with this many unfinished protocol bytes buffered.
    Closed {
        unfinished: usize,
    },
    Other(Refusal),
}

/// A framing failure while the attach response is still outstanding.
///
/// Unlike [`RawPaneSession::closed_before_end`], there is nothing yet to attribute a close to:
/// no keyboard worker is running and no input has been sent, so the plain fact is the whole report.
fn attach_frame_error(error: FrameError) -> Refusal {
    match error {
        // Nothing has been sent that could be refused yet: this is the plain fact.
        FrameError::Closed { unfinished: 0 } => {
            "the native pane socket closed before End".to_string()
        }
        FrameError::Closed { unfinished } => {
            format!("the native pane socket closed with {unfinished} unfinished protocol bytes")
        }
        FrameError::Other(error) => error,
    }
}

impl<W: Write> RawPaneSession<W> {
    /// Attach over `stream`. The caller owns relay signal handling: production acquires
    /// [`RelaySignalGuard`] before this call, so a resize edge published between handler
    /// installation and the geometry read below is kept in `RESIZED` rather than lost.
    fn open_with_io<R: Read + Send + 'static>(
        stream: UnixStream,
        id: AgentId,
        input: R,
        input_fd: Option<std::os::fd::RawFd>,
        output: W,
        geometry: Option<(u16, u16)>,
    ) -> Result<Self, Refusal> {
        #[cfg(test)]
        if let Some(hook) = AFTER_RESIZE_SIGNAL_ACQUIRE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            hook();
        }
        let geometry = match (geometry, input_fd) {
            (Some(geometry), _) => geometry,
            (None, Some(input_fd)) => {
                marion_tui::guard::window_size(input_fd).ok_or_else(|| {
                    "reading native terminal geometry after arming resize tracking".to_string()
                })?
            }
            (None, None) => {
                return Err("native resize tracking requires a terminal input descriptor".into());
            }
        };
        // Reads block and are issued only after `poll(2)` said the socket is readable (see
        // [`Self::wait_for_socket`]), so no read timeout is needed to keep the pump responsive.
        let writer_stream = stream
            .try_clone()
            .map_err(|error| format!("cloning the native pane socket: {error}"))?;
        writer_stream
            .set_write_timeout(Some(WRITE_BOUND))
            .map_err(|error| format!("bounding native pane writes: {error}"))?;
        let writer = Arc::new(std::sync::Mutex::new(writer_stream));
        let mut session = Self {
            stream,
            writer,
            inbound: Vec::new(),
            id,
            output,
            next_seq: 0,
            cut: 0,
            input_fd,
            writable: false,
            leaving: Arc::new(crate::wake::Flag::new()),
            keyboard_failure: Arc::new(std::sync::Mutex::new(None)),
            input_sent: Arc::new(AtomicBool::new(false)),
            keyboard: None,
            nudge: crate::wake::Pipe::new().ok().map(Arc::new),
            status: StatusOverlay::new(geometry),
            detached: Arc::new(AtomicBool::new(false)),
        };
        session.attach(geometry)?;
        session.start_keyboard(input)?;
        Ok(session)
    }

    #[cfg(test)]
    fn open_for_test<R: Read + Send + 'static>(
        stream: UnixStream,
        id: AgentId,
        input: R,
        output: W,
        geometry: (u16, u16),
    ) -> Result<Self, Refusal> {
        Self::open_with_io(stream, id, input, None, output, Some(geometry))
    }

    fn attach(&mut self, (cols, rows): (u16, u16)) -> Result<(), Refusal> {
        self.write_frame(
            &Frame::Request(marion_core::proto::Request::new(
                RequestId::Number(1),
                Call::NodeAttach(marion_core::proto::params::NodeAttachParams {
                    agent_id: self.id.clone(),
                    pane_stream: Some(marion_core::proto::params::PaneStreamCapabilityV1::new()),
                }),
            )),
            "sending native node/attach",
        )?;
        let response = self.await_attach_response()?;
        let body = match response.outcome {
            marion_core::proto::Outcome::Result(body) => body,
            marion_core::proto::Outcome::Error(error) => {
                return Err(format!(
                    "the supervisor refused the native pane attach: {}",
                    error.message
                ));
            }
        };
        let MethodResult::NodeAttach(attached) = marion_core::proto::Method::NodeAttach
            .decode_result(&body)
            .map_err(|error| format!("the native node/attach answer did not decode: {error}"))?
        else {
            return Err("the supervisor answered native node/attach with another result".into());
        };
        let pane = attached
            .pane
            .ok_or_else(|| "the claimed native node has no display plane".to_string())?;
        // `writable: false` has two opposite meanings and the pane says which. A **live** node
        // whose keyboard belongs to another connection is not this claim's pane, and relaying it
        // would put a screen on the operator's terminal that their keystrokes do not drive. A
        // node that has **ended** took nobody's lease: there is nothing left to type into, and
        // the replay this relay attached to carry is exactly what the operator asked for.
        if !pane.writable && !pane.ended {
            return Err("the claimed native connection did not retain its writer lease".into());
        }
        self.writable = pane.writable;
        let ready = pane.pane_ready.ok_or_else(|| {
            "the native pane attach accepted pane-v1 but omitted its Ready descriptor".to_string()
        })?;
        self.cut = ready.cut;

        // The claim's writer lease already exists. Queue the caller's authoritative geometry
        // before opening the replay gate so every later output is ordered behind that resize.
        // A resize is a write, so an ended pane gets none: the supervisor would drop it, and
        // sending it anyway would state a geometry nothing can adopt.
        if self.writable {
            self.send_size(cols, rows)?;
        }
        self.reject_buffered_pre_ready_pane_frames()?;
        self.write_frame(
            &Frame::Input(ClientNotification::new(Input::NodePaneReady(
                NodePaneReadyV1 {
                    agent_id: self.id.clone(),
                    token: ready.token,
                    cut: ready.cut,
                },
            ))),
            "sending native node/pane-ready",
        )
    }

    fn await_attach_response(&mut self) -> Result<marion_core::proto::Response, Refusal> {
        loop {
            let frame = self.next_frame(None, None).map_err(attach_frame_error)?;
            match frame {
                Some(Frame::Response(response)) if response.id == RequestId::Number(1) => {
                    return Ok(response);
                }
                Some(Frame::Response(response)) => {
                    return Err(format!(
                        "the supervisor answered native node/attach with response id {:?}",
                        response.id
                    ));
                }
                Some(Frame::Notification(note))
                    if crate::pane_client::pane_event_targets(&self.id, &note.event) =>
                {
                    return Err(format!(
                        "the supervisor sent node `{}` a pane frame before its native attach response",
                        self.id.0
                    ));
                }
                Some(Frame::Notification(_)) | None => continue,
                Some(other) => {
                    return Err(format!(
                        "the supervisor sent an unexpected frame during native attach: {other:?}"
                    ));
                }
            }
        }
    }

    /// Reject a pane frame which arrived in the same socket read as the attach response. The
    /// server must not open this stream until Ready; silently accepting an already-buffered frame
    /// would make the boundary unenforceable on a fast local socket.
    fn reject_buffered_pre_ready_pane_frames(&mut self) -> Result<(), Refusal> {
        while let Some(end) = self.inbound.iter().position(|byte| *byte == b'\n') {
            if end > crate::serve::MAX_FRAME_BYTES {
                return Err("a pre-Ready native pane frame exceeded the frame bound".into());
            }
            let mut line = self.inbound.drain(..=end).collect::<Vec<_>>();
            line.pop();
            let line = std::str::from_utf8(&line)
                .map_err(|_| "a pre-Ready native protocol frame was not UTF-8".to_string())?;
            let frame = Frame::from_line(line)
                .map_err(|error| format!("a pre-Ready native frame did not decode: {error}"))?;
            match frame {
                Frame::Notification(note)
                    if crate::pane_client::pane_event_targets(&self.id, &note.event) =>
                {
                    return Err(format!(
                        "the supervisor sent node `{}` a pane frame before native Ready",
                        self.id.0
                    ));
                }
                Frame::Notification(_) => {}
                other => {
                    return Err(format!(
                        "the supervisor sent an unexpected frame before native Ready: {other:?}"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Join the keyboard worker before a terminal mode transition so no terminal read overlaps
    /// it. Returns `false` when the worker had already left on its own (operator detach or a read
    /// failure), which the pump must report instead of suspending.
    fn park_keyboard(&mut self) -> bool {
        let parked_by_us = !self.leaving.swap(true, Ordering::SeqCst);
        if let Some(keyboard) = self.keyboard.take() {
            let _ = keyboard.join();
        }
        parked_by_us
    }

    /// Restart the keyboard worker on a fresh terminal input after a parked stop.
    fn resume_keyboard<R: Read + Send + 'static>(&mut self, input: R) -> Result<(), Refusal> {
        self.leaving.store(false, Ordering::SeqCst);
        self.start_keyboard(input)
    }

    fn start_keyboard<R: Read + Send + 'static>(&mut self, mut input: R) -> Result<(), Refusal> {
        let leaving = Arc::clone(&self.leaving);
        let failure = Arc::clone(&self.keyboard_failure);
        let input_sent = Arc::clone(&self.input_sent);
        let writer = Arc::clone(&self.writer);
        let id = self.id.clone();
        let writable = self.writable;
        let status_wanted = Arc::clone(&self.status.wanted);
        let detached = Arc::clone(&self.detached);
        let input_fd = self.input_fd;
        let nudge = self.nudge.clone();
        // The pump blocks in `poll(2)` on the socket. Shutting the read side from here makes it
        // readable at once, so a worker that stops — for any reason — ends the relay now; `pump`
        // reads the failure slot before the EOF it caused.
        let wake = self
            .stream
            .try_clone()
            .map_err(|error| format!("cloning the native pane socket for the keyboard: {error}"))?;
        let stop = move |why: Option<String>| {
            if let Some(why) = why {
                *failure.lock().unwrap_or_else(|error| error.into_inner()) = Some(why);
            }
            leaving.store(true, Ordering::SeqCst);
            let _ = wake.shutdown(std::net::Shutdown::Read);
        };
        let leaving = Arc::clone(&self.leaving);
        self.keyboard = Some(
            std::thread::Builder::new()
                .name("marion-native-keys".into())
                .spawn(move || {
                    let mut keys = Keys::new();
                    let mut bytes = [0u8; 4096];
                    while !leaving.load(Ordering::SeqCst) {
                        let count = match input.read(&mut bytes) {
                            Ok(0) => return stop(None),
                            Ok(count) => count,
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock
                                        | std::io::ErrorKind::Interrupted
                                ) =>
                            {
                                // The terminal's stdin is non-blocking while the relay owns it:
                                // wait for a key or for `leaving`, with no timeout. A fixture's
                                // input has no descriptor and is re-read at the degraded bound.
                                // SAFETY: `input_fd` is the relay terminal's stdin, open for the
                                // whole relay; the borrow ends with the wait.
                                let keyboard = input_fd
                                    .map(|fd| unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) });
                                crate::wake::wait_until(&[keyboard, leaving.fd()], None);
                                continue;
                            }
                            Err(error) => {
                                return stop(Some(format!("reading native pane input: {error}")));
                            }
                        };
                        for action in keys.feed(&bytes[..count]) {
                            match action {
                                Action::Detach => {
                                    detached.store(true, Ordering::SeqCst);
                                    return stop(None);
                                }
                                // The pump paints or clears the row on its next tick; the key
                                // is marion's and never the node's.
                                Action::ToggleStatus => {
                                    status_wanted.fetch_xor(true, Ordering::SeqCst);
                                    if let Some(nudge) = &nudge {
                                        nudge.wake();
                                    }
                                }
                                // A pane that had already ended when this relay attached has
                                // no write half for anyone. The supervisor answers an unleased
                                // keystroke by closing the connection, which would cost the
                                // operator the replay still arriving on it, so the key stops
                                // here. Detach above is unaffected: leaving is not a write.
                                Action::Forward(_) if !writable => {}
                                Action::Forward(bytes) => {
                                    let frame = Frame::Input(ClientNotification::new(
                                        Input::NodePaneWrite(NodePaneWriteV1 {
                                            agent_id: id.clone(),
                                            bytes: marion_core::proto::OpaquePaneBytesV1::new(
                                                bytes,
                                            ),
                                        }),
                                    ));
                                    // Advance before the bytes go out, the same order a frame is
                                    // applied in: the supervisor can close on this keystroke
                                    // before the write even returns here, and the pump reads this
                                    // flag the instant it sees that EOF. A write that then fails
                                    // costs nothing — its own message outranks the flag.
                                    input_sent.store(true, Ordering::SeqCst);
                                    if let Err(error) =
                                        write_serialized(&writer, frame.to_line().as_bytes())
                                    {
                                        return stop(Some(format!(
                                            "sending native pane keyboard input: {error}"
                                        )));
                                    }
                                    #[cfg(test)]
                                    after_keyboard_input_write();
                                }
                            }
                        }
                    }
                })
                .map_err(|error| format!("starting native pane keyboard reader: {error}"))?,
        );
        Ok(())
    }

    /// The worker's failure, if it recorded one: the primary error whenever it is set, because the
    /// EOF the pump then sees is one the worker caused on purpose.
    fn take_keyboard_failure(&self) -> Option<Refusal> {
        self.keyboard_failure
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    /// One frame, or `None` when none arrived by `deadline` (`None`: no deadline) or a signal, a
    /// status toggle or an owned stop (`stop`) woke the wait — each the pump's cue to look at its
    /// flags before waiting again.
    fn next_frame(
        &mut self,
        deadline: Option<std::time::Instant>,
        stop: Option<&crate::wake::SignalWatch>,
    ) -> Result<Option<Frame>, FrameError> {
        loop {
            if let Some(end) = self.inbound.iter().position(|byte| *byte == b'\n') {
                if end > crate::serve::MAX_FRAME_BYTES {
                    return Err(FrameError::Other(format!(
                        "the supervisor sent a native pane frame larger than {} bytes",
                        crate::serve::MAX_FRAME_BYTES
                    )));
                }
                let mut line = self.inbound.drain(..=end).collect::<Vec<_>>();
                line.pop();
                let line = std::str::from_utf8(&line).map_err(|_| {
                    FrameError::Other(
                        "the supervisor sent a non-UTF-8 native protocol frame".to_string(),
                    )
                })?;
                return Frame::from_line(line).map(Some).map_err(|error| {
                    FrameError::Other(format!(
                        "the supervisor sent an unreadable native pane frame: {error}"
                    ))
                });
            }
            if self.inbound.len() > crate::serve::MAX_FRAME_BYTES {
                return Err(FrameError::Other(format!(
                    "the supervisor sent an unterminated native pane frame larger than {} bytes",
                    crate::serve::MAX_FRAME_BYTES
                )));
            }
            if !self.wait_for_socket(deadline, stop) {
                return Ok(None);
            }
            let mut bytes = [0u8; 8192];
            match self.stream.read(&mut bytes) {
                Ok(0) => {
                    return Err(FrameError::Closed {
                        unfinished: self.inbound.len(),
                    });
                }
                Ok(count) => self.inbound.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    return Ok(None);
                }
                Err(error) => {
                    return Err(FrameError::Other(format!(
                        "reading the native pane socket: {error}"
                    )));
                }
            }
        }
    }

    /// Block in `poll(2)` until the socket is readable (`true`), or until `deadline`, a ring of
    /// [`RELAY_WAKE`] or of this session's [`Self::nudge`], or the owned stop's watch (`false`).
    /// Every wake is drained here, before the pump looks at what it is about, so an edge after the
    /// drain wakes the next wait.
    fn wait_for_socket(
        &self,
        deadline: Option<std::time::Instant>,
        stop: Option<&crate::wake::SignalWatch>,
    ) -> bool {
        use std::os::fd::AsFd;
        let wake = relay_wake();
        let nudge = self.nudge.as_deref();
        let mut fds = vec![
            Some(self.stream.as_fd()),
            wake.map(|w| w.fd()),
            nudge.map(|n| n.fd()),
        ];
        if let Some(stop) = stop {
            fds.push(stop.fd());
        }
        let ready = crate::wake::wait_until(&fds, deadline);
        if ready[1]
            && let Some(wake) = wake
        {
            wake.drain();
        }
        if ready[2]
            && let Some(nudge) = nudge
        {
            nudge.drain();
        }
        if ready.get(3).copied().unwrap_or(false)
            && let Some(stop) = stop
        {
            stop.drain();
        }
        ready[0]
    }

    /// What a socket closed before End means, in the order the causes are known: a keyboard
    /// worker that stopped on purpose (its own message, or a clean detach), then the supervisor
    /// refusing the input it was sent, then a connection lost with nothing in flight.
    fn closed_before_end(&self, unfinished: usize) -> Result<RelayStop, Refusal> {
        if let Some(failure) = self.take_keyboard_failure() {
            return Err(failure);
        }
        if self.leaving.load(Ordering::SeqCst) {
            return Ok(RelayStop::Complete);
        }
        if unfinished > 0 {
            return Err(format!(
                "the native pane socket closed with {unfinished} unfinished protocol bytes"
            ));
        }
        if self.input_sent.load(Ordering::SeqCst) {
            return Err(
                "the supervisor refused the relay's keyboard input and closed the native \
                        pane: opaque input it cannot deliver or record is refused rather than \
                        dropped, and the supervisor's log names the reason"
                    .into(),
            );
        }
        Err("the native pane socket closed before End".into())
    }

    /// **Event-driven**: between frames the pump sleeps in `poll(2)` on the socket, the relay
    /// wake (signals, the status toggle) and, while the stop is owned, a watch on `SIGTSTP` — made
    /// here, on the relay thread that blocks it, and before the first look at `sigpending`. Its
    /// only deadline is the status row's [`StatusOverlay::next_due`], so an idle relay makes no
    /// wakeups.
    fn pump(&mut self) -> Result<RelayStop, Refusal> {
        let stop_watch = STOP_OWNED
            .load(Ordering::SeqCst)
            .then(|| crate::wake::SignalWatch::new(SIGTSTP));
        loop {
            if let Some(stop) = self.stop_before_frame()? {
                return Ok(stop);
            }
            self.forward_resize()?;
            self.refresh_status()?;
            let frame = match self.next_frame(self.status.next_due(), stop_watch.as_ref()) {
                Ok(frame) => frame,
                Err(FrameError::Closed { unfinished }) => {
                    return self.closed_before_end(unfinished);
                }
                Err(FrameError::Other(error)) => return Err(error),
            };
            if let Some(stop) = self.apply_frame(frame)? {
                return Ok(stop);
            }
        }
    }

    /// Whether the relay is already over before another frame is read, in the order the reasons
    /// outrank each other: a signal Marion's handler observed, then an owned stop the terminal's
    /// default should see, then a keyboard worker that left — carrying its failure if it had one.
    fn stop_before_frame(&mut self) -> Result<Option<RelayStop>, Refusal> {
        let signal = FIRST_RELAY_SIGNAL.load(Ordering::SeqCst);
        if signal != 0 {
            return Ok(Some(RelayStop::ExternalSignal(signal)));
        }
        if owned_stop_pending()
            .map_err(|error| format!("querying the pending native relay stop: {error}"))?
        {
            return Ok(Some(RelayStop::Suspend));
        }
        if self.leaving.load(Ordering::SeqCst) {
            if let Some(error) = self.take_keyboard_failure() {
                return Err(error);
            }
            return Ok(Some(RelayStop::Complete));
        }
        Ok(None)
    }

    /// One frame from the negotiated stream, onto the operator's terminal. `Some` ends the relay.
    ///
    /// The sequence advances before the bytes are written: the decode has already validated it,
    /// and a write failure is this relay's, not a hole in the supervisor's stream.
    fn apply_frame(&mut self, frame: Option<Frame>) -> Result<Option<RelayStop>, Refusal> {
        let note = match frame {
            Some(Frame::Notification(note)) => note,
            Some(Frame::Response(response)) if response.id == STATUS_SUBSCRIBE_ID => {
                self.absorb_status_subscription(response);
                return Ok(None);
            }
            Some(other) => {
                return Err(format!(
                    "the supervisor sent an unexpected frame during native relay: {other:?}"
                ));
            }
            None => return Ok(None),
        };
        self.status.absorb(&note.event);
        let decoded = crate::pane_client::decode_pane_v1_event(
            &self.id,
            self.next_seq,
            self.cut,
            note.event,
        )?;
        self.next_seq = decoded.next_seq;
        match decoded.action {
            crate::pane_client::PaneV1Action::Output(bytes) => {
                let bytes = bytes.as_bytes();
                self.status.touched = true;
                let marks = self.status.stream.advance(bytes);
                let repairs = self.status.margin_repairs(&marks);
                let mut from = 0;
                for (end, repair) in &repairs {
                    self.output
                        .write_all(&bytes[from..*end])
                        .and_then(|()| self.output.write_all(repair))
                        .map_err(|error| {
                            format!("writing native pane output to the terminal: {error}")
                        })?;
                    from = *end;
                }
                self.output
                    .write_all(&bytes[from..])
                    .and_then(|()| self.output.flush())
                    .map_err(|error| {
                        format!("writing native pane output to the terminal: {error}")
                    })?;
                Ok(None)
            }
            crate::pane_client::PaneV1Action::Resize { .. }
            | crate::pane_client::PaneV1Action::Ignore => Ok(None),
            crate::pane_client::PaneV1Action::End => Ok(Some(RelayStop::Complete)),
        }
    }

    fn forward_resize(&mut self) -> Result<(), Refusal> {
        if !RESIZED.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        let Some(input_fd) = self.input_fd else {
            return Ok(());
        };
        let Some((cols, rows)) = marion_tui::guard::window_size(input_fd) else {
            return Ok(());
        };
        self.status.resized(cols, rows);
        self.send_size(cols, self.status.node_rows())
    }

    /// The status row, on the pump's tick: subscribe at the first request, paint when due, clear
    /// once on toggle-off. A terminal write failure is the relay's, as it is for a pane frame; a
    /// refused or unsendable subscribe is only the row's.
    fn refresh_status(&mut self) -> Result<(), Refusal> {
        let wanted = self.status.wanted.load(Ordering::SeqCst);
        if wanted != self.status.shown {
            self.status.dirty = true;
        }
        if wanted {
            self.subscribe_for_status();
            self.reserve_status_row()?;
        }
        // A pane frame is one pty read and can stop inside the node's escape sequence; marion's
        // bytes there would end it early and print the rest as text. Paint or clear the row once
        // the node's next frame has finished what it started.
        if !self.status.stream.at_boundary() {
            return Ok(());
        }
        if !wanted {
            // Off the terminal first, then back to the node: the full-height resize is the edge
            // the harness repaints its last row on, and a clear after that repaint would erase it.
            if self.status.shown {
                self.status.shown = false;
                self.status.dirty = false;
                self.status.last_drawn = None;
                self.write_status(&status_clear_bytes(self.status.rows))?;
            }
            return self.return_status_row();
        }
        if !self.status.due() || self.status.rows < 2 {
            return Ok(());
        }
        let line = self.status.line(&self.id);
        self.status.shown = true;
        self.status.dirty = false;
        self.status.touched = false;
        self.status.last_drawn = Some(std::time::Instant::now());
        self.write_status(&status_overlay_bytes(
            self.status.rows,
            self.status.cols,
            &line,
        ))
    }

    /// Tell the node its window is one row shorter, once per showing. An ended pane has no write
    /// half and nothing left to paint, so it is only overlaid.
    fn reserve_status_row(&mut self) -> Result<(), Refusal> {
        if !self.writable || self.status.reserved || self.status.rows < 2 {
            return Ok(());
        }
        self.status.reserved = true;
        self.send_size(self.status.cols, self.status.node_rows())
    }

    /// Give the node its full height back.
    fn return_status_row(&mut self) -> Result<(), Refusal> {
        if !self.status.reserved {
            return Ok(());
        }
        self.status.reserved = false;
        self.send_size(self.status.cols, self.status.rows)
    }

    /// Take the row off the terminal and give it back to the node because the relay is leaving
    /// the terminal — a detach, the end, a failure or a stop — whatever the node's stream is in
    /// the middle of: nothing of the node's follows on this terminal to be interrupted. The row is
    /// still wanted, so a relay that resumes after a stop shows it again. The resize is best
    /// effort: a connection that is already gone has no node to tell.
    fn release_status(&mut self) -> Result<(), Refusal> {
        let cleared = if self.status.shown {
            self.status.shown = false;
            self.status.dirty = true;
            self.status.last_drawn = None;
            self.write_status(&status_clear_bytes(self.status.rows))
        } else {
            Ok(())
        };
        let _ = self.return_status_row();
        cleared
    }

    /// The row's `tree/subscribe`, made once at the first request. A refused or unsendable
    /// subscribe is only the row's.
    fn subscribe_for_status(&mut self) {
        if !self.status.subscribed {
            self.status.subscribed = true;
            let sent = self.write_frame(
                &Frame::Request(marion_core::proto::Request::new(
                    STATUS_SUBSCRIBE_ID,
                    Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
                )),
                "sending native tree/subscribe",
            );
            if sent.is_err() {
                self.status.unavailable = true;
            }
        }
    }

    fn write_status(&mut self, bytes: &[u8]) -> Result<(), Refusal> {
        self.output
            .write_all(bytes)
            .and_then(|()| self.output.flush())
            .map_err(|error| format!("writing the marion status row to the terminal: {error}"))
    }

    /// The subscribe answer. A refusal makes the row say so and changes nothing else.
    fn absorb_status_subscription(&mut self, response: marion_core::proto::Response) {
        self.status.dirty = true;
        let body = match response.outcome {
            marion_core::proto::Outcome::Result(body) => body,
            marion_core::proto::Outcome::Error(_) => {
                self.status.unavailable = true;
                return;
            }
        };
        match marion_core::proto::Method::TreeSubscribe.decode_result(&body) {
            Ok(MethodResult::TreeSubscribe(snapshot)) => self.status.nodes = Some(snapshot.nodes),
            _ => self.status.unavailable = true,
        }
    }

    fn send_size(&self, cols: u16, rows: u16) -> Result<(), Refusal> {
        self.write_frame(
            &Frame::Input(ClientNotification::new(Input::NodeResize {
                agent_id: self.id.clone(),
                cols,
                rows,
            })),
            "sending native node/resize",
        )
    }

    fn write_frame(&self, frame: &Frame, action: &str) -> Result<(), Refusal> {
        write_serialized(&self.writer, frame.to_line().as_bytes())
            .map_err(|error| format!("{action}: {error}"))
    }
}

/// The status line's text: the native node's **direct** children, counted by state. Live is
/// [`crate::tree::running`]'s rule, so this line and `marion tree`'s status row never disagree
/// about which nodes are still marion's processes; of those, a `NodeState::Blocked` child is shown
/// in its own column rather than as running, because a blocked child is the one an operator has
/// to act on. Done is `Exited(Ok)`; every other exit is failed. The columns are disjoint.
///
/// Then, only when non-zero, **attention over the whole subtree** by [`crate::tree::attention_of`]:
/// the children's columns cannot show a grandchild that failed, and that is the node an operator
/// inside a native session has no other way to hear about.
fn status_line(id: &AgentId, nodes: &[marion_core::proto::NodeSummary]) -> String {
    use marion_core::contract::ExitStatus;
    use marion_core::node::NodeState;
    let children: Vec<marion_core::proto::NodeSummary> = nodes
        .iter()
        .filter(|node| node.parent_id.as_ref() == Some(id))
        .cloned()
        .collect();
    let (mut blocked, mut done, mut failed) = (0usize, 0usize, 0usize);
    for child in &children {
        match child.state {
            NodeState::Blocked(_) => blocked += 1,
            NodeState::Exited(ExitStatus::Ok) => done += 1,
            NodeState::Exited(_) => failed += 1,
            _ => {}
        }
    }
    // A blocked node is not exited, so `running` counts it; it is moved to its own column here.
    let running = crate::tree::running(&children).saturating_sub(blocked);
    let attention = crate::tree::attention_count(&descendants(id, nodes));
    let attention = match attention {
        0 => String::new(),
        n => format!(" · attention {n}"),
    };
    format!(
        "marion: {} children · running {running} · blocked {blocked} · done {done} · failed {failed}{attention}",
        children.len()
    )
}

/// Every node below `id` in `nodes`, each once. The visited set starts with `id` itself, so a
/// parent cycle — which §7.5's immutable parent should forbid and this refuses to trust — can
/// neither loop the walk nor count the native node as its own descendant.
fn descendants(
    id: &AgentId,
    nodes: &[marion_core::proto::NodeSummary],
) -> Vec<marion_core::proto::NodeSummary> {
    let mut seen = std::collections::HashSet::from([id.clone()]);
    let mut frontier = vec![id.clone()];
    let mut out = Vec::new();
    while let Some(parent) = frontier.pop() {
        for node in nodes {
            if node.parent_id.as_ref() == Some(&parent) && seen.insert(node.agent_id.clone()) {
                frontier.push(node.agent_id.clone());
                out.push(node.clone());
            }
        }
    }
    out
}

/// One row, restored around: `ESC 7` saves the cursor, `CSI 1;rows-1 r` stops the scroll region
/// above the last row so the node's scrolling never carries the row into its text, `CSI rows;1H`
/// goes to the last row, `CSI 2K` clears it, the text is painted in reverse video and reset, and
/// `ESC 8` puts the cursor back where the node left it (DECSTBM homes it). Nothing here touches
/// `?1049`, so the overlay is the same bytes on the main and alternate screens alike. The region is
/// restated on every paint, so a reset the relay did not see is repaired at the next one.
fn status_overlay_bytes(rows: u16, cols: u16, line: &str) -> Vec<u8> {
    let shown: String = line.chars().take(usize::from(cols)).collect();
    let mut bytes = Vec::with_capacity(shown.len() + 40);
    bytes.extend_from_slice(b"\x1b7");
    bytes.extend_from_slice(format!("\x1b[1;{}r", rows.saturating_sub(1)).as_bytes());
    bytes.extend_from_slice(format!("\x1b[{rows};1H").as_bytes());
    bytes.extend_from_slice(b"\x1b[2K\x1b[7m");
    bytes.extend_from_slice(shown.as_bytes());
    bytes.extend_from_slice(b"\x1b[0m\x1b8");
    bytes
}

/// What toggling the line off leaves: the whole screen as the scroll region again, the row
/// cleared, and the cursor restored.
fn status_clear_bytes(rows: u16) -> Vec<u8> {
    format!("\x1b7\x1b[r\x1b[{rows};1H\x1b[2K\x1b8").into_bytes()
}

impl<W: Write> Drop for RawPaneSession<W> {
    fn drop(&mut self) {
        self.leaving.store(true, Ordering::SeqCst);
        if let Some(keyboard) = self.keyboard.take() {
            let _ = keyboard.join();
        }
    }
}

#[cfg(test)]
mod tests;
