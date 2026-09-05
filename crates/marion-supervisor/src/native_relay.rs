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
use marion_proto::{
    Call, ClientNotification, Event, Frame, Input, MethodResult, NodePaneReadyV1, NodePaneWriteV1,
    RequestId,
};
use marion_tui::{Action, Keys};

type Refusal = String;

const POLL: std::time::Duration = std::time::Duration::from_millis(50);
static RESIZED: AtomicBool = AtomicBool::new(false);
static RELAY_SIGNAL_EVENTS: AtomicU32 = AtomicU32::new(0);
static FIRST_RELAY_SIGNAL: AtomicI32 = AtomicI32::new(0);
static RELAY_SIGNAL_OWNER: Mutex<()> = Mutex::new(());
static RELAY_SIGNAL_OWNERSHIP_POISONED: AtomicBool = AtomicBool::new(false);
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
}

struct PriorSignalAction {
    signal: std::ffi::c_int,
    action: Sigaction,
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
        // No signal edge from a prior owner may leak into this session.
        RESIZED.store(false, Ordering::SeqCst);
        RELAY_SIGNAL_EVENTS.store(0, Ordering::SeqCst);
        FIRST_RELAY_SIGNAL.store(0, Ordering::SeqCst);
        #[cfg(test)]
        RELAY_SIGNAL_INSTALL_ATTEMPT.store(0, Ordering::SeqCst);
        let action = Sigaction::relay_handler();
        let mut prior = Vec::with_capacity(HANDLED_SIGNALS.len());
        for signal in HANDLED_SIGNALS {
            let mut prior_action = Sigaction::zeroed();
            if let Err(error) = install_relay_signal_action(signal, &action, &mut prior_action) {
                return match restore_signal_actions_matching(&mut prior, |_| true) {
                    Ok(()) => {
                        clear_relay_signal_state();
                        Err(format!(
                            "installing process signal action {signal}: {error}"
                        ))
                    }
                    Err(rollback) => {
                        poison_relay_signal_ownership();
                        Err(format!(
                            "installing process signal action {signal}: {error}; rolling back partially installed process signal actions failed: {rollback}"
                        ))
                    }
                };
            }
            prior.push(PriorSignalAction {
                signal,
                action: prior_action,
            });
        }
        if stop_owned && let Err(error) = change_thread_signal_mask(SIG_BLOCK, SIGTSTP) {
            return match restore_signal_actions_matching(&mut prior, |_| true) {
                Ok(()) => {
                    clear_relay_signal_state();
                    Err(format!(
                        "blocking SIGTSTP on the native relay thread: {error}"
                    ))
                }
                Err(rollback) => {
                    poison_relay_signal_ownership();
                    Err(format!(
                        "blocking SIGTSTP on the native relay thread: {error}; rolling back process signal actions failed: {rollback}"
                    ))
                }
            };
        }
        Ok(Self {
            prior,
            stop_owned,
            captured_signal: None,
            _owner: owner,
        })
    }

    /// Whether this relay owns the terminal's default stop: `SIGTSTP` was default and unblocked
    /// at acquisition, so it now stays blocked and is only ever observed pending.
    #[cfg(test)]
    pub(crate) fn owns_stop(&self) -> bool {
        self.stop_owned
    }

    /// Whether an owned `SIGTSTP` is pending. Observation never consumes it.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the relay pump consumes the pending stop in the suspend transition"
        )
    )]
    pub(crate) fn stop_pending(&self) -> std::io::Result<bool> {
        if !self.stop_owned {
            return Ok(false);
        }
        pending_signals().map(|pending| signal_in_set(&pending, SIGTSTP))
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
        }
        clear_relay_signal_state();
        Ok(self.captured_signal.take())
    }
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
    matches!(signal, SIGINT | SIGTERM | SIGHUP)
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
}

struct OwnedFdReader(std::os::fd::OwnedFd);

impl Read for OwnedFdReader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        rustix::io::read(&self.0, bytes).map_err(Into::into)
    }
}

struct OwnedFdWriter(std::os::fd::OwnedFd);

impl Write for OwnedFdWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        rustix::io::write(&self.0, bytes).map_err(Into::into)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Claim the authenticated launch on its issuing socket and transparently relay its pane until
/// End or operator detach. The terminal guard remains live across every protocol and I/O failure.
#[expect(
    dead_code,
    reason = "compiled for review and tests while production descriptors and dispatch stay dark"
)]
pub(crate) fn run(handoff: crate::native_bootstrap::NativeFacadeHandoff) -> Result<(), Refusal> {
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
) -> Result<(), Refusal> {
    let primary = (|| {
        let input_fd = {
            use std::os::fd::AsRawFd;
            terminal.stdin().as_raw_fd()
        };
        let stdin = rustix::io::fcntl_dupfd_cloexec(terminal.stdin(), 3)
            .map_err(|error| format!("retaining native terminal input: {error}"))?;
        let stdout = rustix::io::fcntl_dupfd_cloexec(terminal.stdout(), 3)
            .map_err(|error| format!("retaining native terminal output: {error}"))?;
        let mut session = RawPaneSession::open_with_io(
            stream,
            agent_id,
            OwnedFdReader(stdin),
            Some(input_fd),
            OwnedFdWriter(stdout),
            None,
        )?;
        #[cfg(test)]
        if std::env::var_os("MARION_NATIVE_RELAY_PROBE").is_some() {
            eprintln!("NATIVE_RELAY_READY");
        }
        let result = session.pump();
        drop(session);
        result
    })();
    finish_claimed_relay(terminal, primary, signals)
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
    struct BorrowedTerminalWriter<'a>(std::os::fd::BorrowedFd<'a>);
    impl Write for BorrowedTerminalWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            rustix::io::write(self.0, bytes).map_err(Into::into)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    write_passive_terminal_cleanup_to(BorrowedTerminalWriter(terminal.stdout()))
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
    leaving: Arc<AtomicBool>,
    keyboard_failure: Arc<std::sync::Mutex<Option<String>>>,
    keyboard: Option<std::thread::JoinHandle<()>>,
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
        stream
            .set_read_timeout(Some(POLL))
            .map_err(|error| format!("bounding native pane reads: {error}"))?;
        let writer_stream = stream
            .try_clone()
            .map_err(|error| format!("cloning the native pane socket: {error}"))?;
        writer_stream
            .set_write_timeout(Some(POLL))
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
            leaving: Arc::new(AtomicBool::new(false)),
            keyboard_failure: Arc::new(std::sync::Mutex::new(None)),
            keyboard: None,
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
            &Frame::Request(marion_proto::Request::new(
                RequestId::Number(1),
                Call::NodeAttach(marion_proto::params::NodeAttachParams {
                    agent_id: self.id.clone(),
                    pane_stream: Some(marion_proto::params::PaneStreamCapabilityV1::new()),
                }),
            )),
            "sending native node/attach",
        )?;
        let response = self.await_attach_response()?;
        let body = match response.outcome {
            marion_proto::Outcome::Result(body) => body,
            marion_proto::Outcome::Error(error) => {
                return Err(format!(
                    "the supervisor refused the native pane attach: {}",
                    error.message
                ));
            }
        };
        let MethodResult::NodeAttach(attached) = marion_proto::Method::NodeAttach
            .decode_result(&body)
            .map_err(|error| format!("the native node/attach answer did not decode: {error}"))?
        else {
            return Err("the supervisor answered native node/attach with another result".into());
        };
        let pane = attached
            .pane
            .ok_or_else(|| "the claimed native node has no display plane".to_string())?;
        if !pane.writable {
            return Err("the claimed native connection did not retain its writer lease".into());
        }
        let ready = pane.pane_ready.ok_or_else(|| {
            "the native pane attach accepted pane-v1 but omitted its Ready descriptor".to_string()
        })?;
        self.cut = ready.cut;

        // The claim's writer lease already exists. Queue the caller's authoritative geometry
        // before opening the replay gate so every later output is ordered behind that resize.
        self.send_size(cols, rows)?;
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

    fn await_attach_response(&mut self) -> Result<marion_proto::Response, Refusal> {
        loop {
            match self.next_frame()? {
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
                    if matches!(&note.event,
                        Event::NodePaneFrame(frame) if frame.agent_id == self.id)
                        || matches!(&note.event,
                            Event::NodePty { agent_id, .. } if agent_id == &self.id) =>
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
                    if matches!(&note.event,
                        Event::NodePaneFrame(frame) if frame.agent_id == self.id)
                        || matches!(&note.event,
                            Event::NodePty { agent_id, .. } if agent_id == &self.id) =>
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

    fn start_keyboard<R: Read + Send + 'static>(&mut self, mut input: R) -> Result<(), Refusal> {
        let leaving = Arc::clone(&self.leaving);
        let failure = Arc::clone(&self.keyboard_failure);
        let writer = Arc::clone(&self.writer);
        let id = self.id.clone();
        self.keyboard = Some(
            std::thread::Builder::new()
                .name("marion-native-keys".into())
                .spawn(move || {
                    let mut keys = Keys::new();
                    let mut bytes = [0u8; 4096];
                    while !leaving.load(Ordering::SeqCst) {
                        let count = match input.read(&mut bytes) {
                            Ok(0) => {
                                leaving.store(true, Ordering::SeqCst);
                                return;
                            }
                            Ok(count) => count,
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock
                                        | std::io::ErrorKind::Interrupted
                                ) =>
                            {
                                std::thread::sleep(POLL);
                                continue;
                            }
                            Err(error) => {
                                *failure.lock().unwrap_or_else(|error| error.into_inner()) =
                                    Some(format!("reading native pane input: {error}"));
                                leaving.store(true, Ordering::SeqCst);
                                return;
                            }
                        };
                        for action in keys.feed(&bytes[..count]) {
                            match action {
                                Action::Detach => {
                                    leaving.store(true, Ordering::SeqCst);
                                    return;
                                }
                                Action::Forward(bytes) => {
                                    let frame = Frame::Input(ClientNotification::new(
                                        Input::NodePaneWrite(NodePaneWriteV1 {
                                            agent_id: id.clone(),
                                            bytes: marion_proto::OpaquePaneBytesV1::new(bytes),
                                        }),
                                    ));
                                    if let Err(error) =
                                        write_serialized(&writer, frame.to_line().as_bytes())
                                    {
                                        *failure
                                            .lock()
                                            .unwrap_or_else(|error| error.into_inner()) = Some(
                                            format!("sending native pane keyboard input: {error}"),
                                        );
                                        leaving.store(true, Ordering::SeqCst);
                                        return;
                                    }
                                }
                            }
                        }
                    }
                })
                .map_err(|error| format!("starting native pane keyboard reader: {error}"))?,
        );
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<Frame>, Refusal> {
        loop {
            if let Some(end) = self.inbound.iter().position(|byte| *byte == b'\n') {
                if end > crate::serve::MAX_FRAME_BYTES {
                    return Err(format!(
                        "the supervisor sent a native pane frame larger than {} bytes",
                        crate::serve::MAX_FRAME_BYTES
                    ));
                }
                let mut line = self.inbound.drain(..=end).collect::<Vec<_>>();
                line.pop();
                let line = std::str::from_utf8(&line).map_err(|_| {
                    "the supervisor sent a non-UTF-8 native protocol frame".to_string()
                })?;
                return Frame::from_line(line).map(Some).map_err(|error| {
                    format!("the supervisor sent an unreadable native pane frame: {error}")
                });
            }
            if self.inbound.len() > crate::serve::MAX_FRAME_BYTES {
                return Err(format!(
                    "the supervisor sent an unterminated native pane frame larger than {} bytes",
                    crate::serve::MAX_FRAME_BYTES
                ));
            }
            let mut bytes = [0u8; 8192];
            match self.stream.read(&mut bytes) {
                Ok(0) if self.inbound.is_empty() => {
                    return Err("the native pane socket closed before End".into());
                }
                Ok(0) => {
                    return Err(format!(
                        "the native pane socket closed with {} unfinished protocol bytes",
                        self.inbound.len()
                    ));
                }
                Ok(count) => self.inbound.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(format!("reading the native pane socket: {error}")),
            }
        }
    }

    fn pump(&mut self) -> Result<RelayStop, Refusal> {
        loop {
            let signal = FIRST_RELAY_SIGNAL.load(Ordering::SeqCst);
            if signal != 0 {
                return Ok(RelayStop::ExternalSignal(signal));
            }
            if self.leaving.load(Ordering::SeqCst) {
                if let Some(error) = self
                    .keyboard_failure
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take()
                {
                    return Err(error);
                }
                return Ok(RelayStop::Complete);
            }
            self.forward_resize()?;
            match self.next_frame()? {
                Some(Frame::Notification(note)) => {
                    let decoded = crate::pane_client::decode_pane_v1_event(
                        &self.id,
                        self.next_seq,
                        self.cut,
                        note.event,
                    )?;
                    self.next_seq = decoded.next_seq;
                    match decoded.action {
                        crate::pane_client::PaneV1Action::Output(bytes) => {
                            self.output
                                .write_all(bytes.as_bytes())
                                .and_then(|()| self.output.flush())
                                .map_err(|error| {
                                    format!("writing native pane output to the terminal: {error}")
                                })?;
                        }
                        crate::pane_client::PaneV1Action::Resize { .. }
                        | crate::pane_client::PaneV1Action::Ignore => {}
                        crate::pane_client::PaneV1Action::End => return Ok(RelayStop::Complete),
                    }
                }
                Some(other) => {
                    return Err(format!(
                        "the supervisor sent an unexpected frame during native relay: {other:?}"
                    ));
                }
                None => {}
            }
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
        self.send_size(cols, rows)
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

impl<W: Write> Drop for RawPaneSession<W> {
    fn drop(&mut self) {
        self.leaving.store(true, Ordering::SeqCst);
        if let Some(keyboard) = self.keyboard.take() {
            let _ = keyboard.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};

    use marion_core::contract::AgentId;
    use marion_proto::{Call, Event, Frame, Input, MethodResult, PaneFrameKindV1, RequestId};

    use super::{
        AFTER_RELAY_SIGNAL_RESTORE, AFTER_RESIZE_SIGNAL_ACQUIRE,
        BEFORE_REDELIVERY_WHILE_OWNER_HELD, FIRST_RELAY_SIGNAL, HANDLED_SIGNALS, HUNG_UP,
        INTERRUPTED, RELAY_SIGNAL_EVENTS, RESIZED, RawPaneSession, RelaySignalGuard, RelayStop,
        SIG_BLOCK, SIG_DFL, SIG_IGN, SIGHUP, SIGINT, SIGTERM, SIGTSTP, SIGWINCH, SigSet, Sigaction,
        TERMINATED, current_thread_signal_mask, empty_sigset, fail_relay_signal_install_at,
        fail_relay_signal_restore_at, fail_relay_signal_restore_at_attempts, finish_claimed_relay,
        on_relay_signal, pthread_sigmask, raise, redeliver_signal, relay_claimed,
        resolve_relay_finish, setup_after_signal_acquire, sigaction, signal, signal_in_set,
        signal_set, write_passive_terminal_cleanup_to,
    };
    use crate::native_tty::test_support::{
        RawRelayTerminal, fail_next_stdin_flag_restore, raw_relay_terminal_on_test_pty,
    };

    const SIGNAL_RESTORE_PROBE: &str = "MARION_NATIVE_SIGNAL_RESTORE_PROBE";
    const SIGNAL_CONTENTION_PROBE: &str = "MARION_NATIVE_SIGNAL_CONTENTION_PROBE";
    const SIGNAL_RESET_PROBE: &str = "MARION_NATIVE_SIGNAL_RESET_PROBE";
    const SIGNAL_OWNER_PROBE: &str = "MARION_NATIVE_SIGNAL_OWNER_PROBE";
    const SIGNAL_HANDLER_PROBE: &str = "MARION_NATIVE_SIGNAL_HANDLER_PROBE";
    const SIGNAL_IGNORED_PROBE: &str = "MARION_NATIVE_SIGNAL_IGNORED_PROBE";
    const SIGNAL_SYNC_REDELIVERY_PROBE: &str = "MARION_NATIVE_SIGNAL_SYNC_REDELIVERY_PROBE";
    const SIGNAL_TIMEOUT_PROBE: &str = "MARION_NATIVE_SIGNAL_TIMEOUT_PROBE";
    const PTY_SMOKE_INNER: &str = "MARION_NATIVE_PTY_SMOKE_INNER";
    const PTY_SMOKE_PROBE: &str = "MARION_NATIVE_PTY_SMOKE_PROBE";
    const PTY_SIGTERM_INNER: &str = "MARION_NATIVE_PTY_SIGTERM_INNER";
    const PTY_SIGTERM_PROBE: &str = "MARION_NATIVE_PTY_SIGTERM_PROBE";
    const PTY_NESTED_RUNNER_STARTUP_BOUND: Duration = Duration::from_secs(3);
    const PTY_SMOKE_LIFECYCLE_BOUND: Duration = Duration::from_secs(3);
    const PTY_SIGTERM_READY_BOUND: Duration = Duration::from_secs(3);
    const PTY_SIGTERM_OBSERVED_BOUND: Duration = Duration::from_secs(3);
    const PTY_SIGTERM_NATURAL_EXIT_BOUND: Duration = Duration::from_millis(500);
    const SIG_ERR: usize = usize::MAX;
    static SENTINEL_HITS: AtomicUsize = AtomicUsize::new(0);

    #[cfg(target_os = "macos")]
    const TEST_SA_RESTART: i32 = 0x0002;
    #[cfg(target_os = "linux")]
    const TEST_SA_RESTART: i32 = 0x1000_0000;

    fn pty_smoke_outer_bound() -> Duration {
        PTY_NESTED_RUNNER_STARTUP_BOUND + PTY_SMOKE_LIFECYCLE_BOUND
    }

    fn pty_sigterm_outer_bound() -> Duration {
        PTY_NESTED_RUNNER_STARTUP_BOUND
            + PTY_SIGTERM_READY_BOUND
            + PTY_SIGTERM_OBSERVED_BOUND
            + PTY_SIGTERM_NATURAL_EXIT_BOUND
    }

    unsafe extern "C" {
        fn _exit(status: std::ffi::c_int) -> !;
    }

    #[cfg(target_os = "macos")]
    const SIG_SETMASK: std::ffi::c_int = 3;
    #[cfg(target_os = "linux")]
    const SIG_SETMASK: std::ffi::c_int = 2;

    fn signal_added_to_set(mut set: SigSet, signal: std::ffi::c_int) -> SigSet {
        #[cfg(target_os = "macos")]
        {
            set |= signal_set(signal);
        }
        #[cfg(target_os = "linux")]
        {
            let signal = signal_set(signal);
            for (word, signal_word) in set.iter_mut().zip(signal) {
                *word |= signal_word;
            }
        }
        set
    }

    struct RestoreThreadSignalMask(SigSet);

    impl Drop for RestoreThreadSignalMask {
        fn drop(&mut self) {
            // SAFETY: the saved mask was returned for this calling thread by pthread_sigmask.
            let _ = unsafe { pthread_sigmask(SIG_SETMASK, &self.0, std::ptr::null_mut()) };
        }
    }

    fn block_signal_on_current_thread(signal: std::ffi::c_int) -> RestoreThreadSignalMask {
        let blocked = signal_set(signal);
        let mut prior = empty_sigset();
        // SAFETY: both pointers refer to valid platform `sigset_t` representations.
        assert_eq!(
            unsafe { pthread_sigmask(SIG_BLOCK, &blocked, &mut prior) },
            0
        );
        RestoreThreadSignalMask(prior)
    }

    extern "C" fn sentinel_winch(_: std::ffi::c_int) {
        SENTINEL_HITS.fetch_add(1, Ordering::SeqCst);
    }

    struct RestoreSignal(usize);

    impl Drop for RestoreSignal {
        fn drop(&mut self) {
            // SAFETY: this writes back the handler returned by `signal` for the same signal.
            unsafe { signal(SIGWINCH, self.0) };
        }
    }

    fn install_sentinel() -> RestoreSignal {
        SENTINEL_HITS.store(0, Ordering::SeqCst);
        // SAFETY: the handler performs one lock-free atomic increment.
        let prior = unsafe { signal(SIGWINCH, sentinel_winch as *const () as usize) };
        assert_ne!(prior, SIG_ERR, "installing the SIGWINCH test sentinel");
        RestoreSignal(prior)
    }

    struct RestoreActions(Vec<(std::ffi::c_int, Sigaction)>);

    impl Drop for RestoreActions {
        fn drop(&mut self) {
            for (signal, prior) in self.0.iter().rev() {
                // SAFETY: each action is the unmodified value returned by `sigaction` for the
                // same signal, and the isolated probe has no concurrent signal owner.
                let _ = unsafe { sigaction(*signal, prior, std::ptr::null_mut()) };
            }
        }
    }

    fn install_exact_sentinels() -> RestoreActions {
        let mut priors = Vec::new();
        for signal in HANDLED_SIGNALS {
            let action = Sigaction {
                handler: sentinel_winch as *const () as usize,
                mask: sentinel_sigset(),
                flags: TEST_SA_RESTART,
                #[cfg(target_os = "linux")]
                restorer: 0,
            };
            let mut prior = Sigaction::zeroed();
            // SAFETY: `action` and `prior` use libc's Darwin/Linux `struct sigaction` layout.
            assert_eq!(unsafe { sigaction(signal, &action, &mut prior) }, 0);
            priors.push((signal, prior));
        }
        RestoreActions(priors)
    }

    fn install_ignored_action(signal: std::ffi::c_int) -> RestoreActions {
        let action = Sigaction {
            handler: SIG_IGN,
            mask: empty_sigset(),
            flags: 0,
            #[cfg(target_os = "linux")]
            restorer: 0,
        };
        let mut prior = Sigaction::zeroed();
        // SAFETY: `action` and `prior` use libc's Darwin/Linux `struct sigaction` layout.
        assert_eq!(unsafe { sigaction(signal, &action, &mut prior) }, 0);
        RestoreActions(vec![(signal, prior)])
    }

    #[cfg(target_os = "macos")]
    fn sentinel_sigset() -> super::SigSet {
        1 << (SIGTERM - 1)
    }

    #[cfg(target_os = "linux")]
    fn sentinel_sigset() -> super::SigSet {
        let mut mask = empty_sigset();
        let bit = usize::try_from(SIGTERM - 1).expect("SIGTERM is positive");
        mask[bit / usize::BITS as usize] |= 1usize << (bit % usize::BITS as usize);
        mask
    }

    fn snapshot_actions() -> Vec<Sigaction> {
        HANDLED_SIGNALS
            .iter()
            .map(|signal| {
                let mut action = Sigaction::zeroed();
                // SAFETY: a null replacement queries the current action into a valid out pointer.
                assert_eq!(
                    unsafe { sigaction(*signal, std::ptr::null(), &mut action) },
                    0
                );
                action
            })
            .collect()
    }

    fn snapshot_action(signal: std::ffi::c_int) -> Sigaction {
        let mut action = Sigaction::zeroed();
        // SAFETY: a null replacement queries the current action into a valid out pointer.
        assert_eq!(
            unsafe { sigaction(signal, std::ptr::null(), &mut action) },
            0
        );
        action
    }

    fn assert_same_actions(actual: &[Sigaction], expected: &[Sigaction]) {
        assert_eq!(actual.len(), expected.len());
        for (signal, (actual, expected)) in HANDLED_SIGNALS
            .iter()
            .zip(actual.iter().zip(expected.iter()))
        {
            assert_eq!(actual.handler, expected.handler, "handler for {signal}");
            assert_eq!(actual.mask, expected.mask, "mask for {signal}");
            assert_eq!(actual.flags, expected.flags, "flags for {signal}");
            #[cfg(target_os = "linux")]
            assert_eq!(actual.restorer, expected.restorer, "restorer for {signal}");
        }
    }

    fn raise_winch() {
        // SAFETY: SIGWINCH is handled by either the relay's atomic-only handler or the test's
        // atomic-only sentinel throughout these isolated child probes.
        assert_eq!(unsafe { raise(SIGWINCH) }, 0);
    }

    #[test]
    fn relay_signal_guard_restores_every_prior_action_and_reacquires() {
        if !run_isolated_signal_probe(
            SIGNAL_OWNER_PROBE,
            "relay_signal_guard_restores_every_prior_action_and_reacquires",
        ) {
            return;
        }
        let _restore_originals = install_exact_sentinels();
        let sentinels = snapshot_actions();
        assert!(
            sentinels.iter().all(|action| action.mask != empty_sigset()),
            "the exact-action oracle requires a non-default signal mask"
        );
        assert!(
            sentinels
                .iter()
                .all(|action| action.flags & TEST_SA_RESTART != 0),
            "the exact-action oracle requires a non-default action flag"
        );

        let first = RelaySignalGuard::acquire().expect("the first relay owns every signal");
        assert!(
            RelaySignalGuard::acquire().is_err(),
            "overlapping process-global signal ownership was admitted"
        );
        drop(first);
        assert_same_actions(&snapshot_actions(), &sentinels);

        drop(RelaySignalGuard::acquire().expect("signal ownership is reacquirable after drop"));
        assert_same_actions(&snapshot_actions(), &sentinels);
    }

    #[test]
    fn relay_signal_guard_rolls_back_a_partial_install_and_reacquires() {
        if !run_isolated_signal_probe(
            SIGNAL_OWNER_PROBE,
            "relay_signal_guard_rolls_back_a_partial_install_and_reacquires",
        ) {
            return;
        }
        let _restore_originals = install_exact_sentinels();
        let sentinels = snapshot_actions();
        fail_relay_signal_install_at(3);

        let error = RelaySignalGuard::acquire()
            .err()
            .expect("the injected third signal installation fails");
        assert!(
            error.contains("injected signal action installation failure"),
            "{error}"
        );
        assert_same_actions(&snapshot_actions(), &sentinels);

        drop(RelaySignalGuard::acquire().expect("ownership is reacquirable after rollback"));
        assert_same_actions(&snapshot_actions(), &sentinels);
    }

    #[test]
    fn partial_install_and_rollback_failure_poison_signal_ownership() {
        if !run_isolated_signal_probe(
            SIGNAL_OWNER_PROBE,
            "partial_install_and_rollback_failure_poison_signal_ownership",
        ) {
            return;
        }
        let _restore_originals = install_exact_sentinels();
        fail_relay_signal_install_at(3);
        fail_relay_signal_restore_at(1);

        let error = RelaySignalGuard::acquire()
            .err()
            .expect("installation and rollback both fail");
        assert!(
            error.contains("injected signal action installation failure"),
            "the install failure was lost: {error}"
        );
        assert!(
            error.contains("injected signal action restoration failure"),
            "the rollback failure was lost: {error}"
        );
        let reacquire = RelaySignalGuard::acquire()
            .err()
            .expect("unsafe process-global ownership stays poisoned");
        assert!(reacquire.contains("poisoned"), "{reacquire}");
    }

    #[test]
    fn relay_signal_handlers_only_publish_atomic_events() {
        if !run_isolated_signal_probe(
            SIGNAL_HANDLER_PROBE,
            "relay_signal_handlers_only_publish_atomic_events",
        ) {
            return;
        }
        let _restore_originals = install_exact_sentinels();
        SENTINEL_HITS.store(0, Ordering::SeqCst);
        RESIZED.store(false, Ordering::SeqCst);
        RELAY_SIGNAL_EVENTS.store(0, Ordering::SeqCst);
        let guard = RelaySignalGuard::acquire().expect("the relay owns every signal");

        for signal in HANDLED_SIGNALS {
            // SAFETY: every signal has the relay's atomic-only handler throughout this loop.
            assert_eq!(unsafe { raise(signal) }, 0, "raising signal {signal}");
        }

        assert!(RESIZED.load(Ordering::SeqCst), "SIGWINCH was not published");
        assert_eq!(
            RELAY_SIGNAL_EVENTS.load(Ordering::SeqCst),
            INTERRUPTED | TERMINATED | HUNG_UP,
            "relay signal handlers did not publish every event"
        );
        assert_eq!(
            SENTINEL_HITS.load(Ordering::SeqCst),
            0,
            "a prior signal action ran while the relay owned the process actions"
        );
        drop(guard);
        let deadline = Instant::now() + Duration::from_secs(1);
        while SENTINEL_HITS.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(
            SENTINEL_HITS.load(Ordering::SeqCst),
            1,
            "the guard did not finish asynchronous first-signal redelivery before test teardown"
        );
    }

    #[test]
    fn relay_signal_handler_preserves_the_first_external_signal_and_guard_resets_it() {
        if !run_isolated_signal_probe(
            SIGNAL_HANDLER_PROBE,
            "relay_signal_handler_preserves_the_first_external_signal_and_guard_resets_it",
        ) {
            return;
        }
        let _restore_originals = install_exact_sentinels();
        SENTINEL_HITS.store(0, Ordering::SeqCst);
        let guard = RelaySignalGuard::acquire().expect("the relay owns every signal");
        on_relay_signal(SIGTERM);
        on_relay_signal(SIGINT);
        on_relay_signal(SIGHUP);
        assert_eq!(FIRST_RELAY_SIGNAL.load(Ordering::SeqCst), SIGTERM);
        drop(guard);
        assert_eq!(FIRST_RELAY_SIGNAL.load(Ordering::SeqCst), 0);
        let deadline = Instant::now() + Duration::from_secs(1);
        while SENTINEL_HITS.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(SENTINEL_HITS.load(Ordering::SeqCst), 1);

        let guard = RelaySignalGuard::acquire().expect("a later relay reacquires signals");
        assert_eq!(FIRST_RELAY_SIGNAL.load(Ordering::SeqCst), 0);
        drop(guard);
        assert_eq!(SENTINEL_HITS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn relay_signal_guard_preserves_an_ignored_sigterm_disposition() {
        if !run_isolated_signal_probe(
            SIGNAL_IGNORED_PROBE,
            "relay_signal_guard_preserves_an_ignored_sigterm_disposition",
        ) {
            return;
        }
        let _restore_original = install_ignored_action(SIGTERM);
        let guard = RelaySignalGuard::acquire().expect("the relay owns every signal");
        on_relay_signal(SIGTERM);
        drop(guard);

        assert_eq!(FIRST_RELAY_SIGNAL.load(Ordering::SeqCst), 0);
        let ignored = snapshot_action(SIGTERM);
        assert_eq!(
            ignored.handler, SIG_IGN,
            "SIGTERM no longer remains ignored"
        );
        drop(RelaySignalGuard::acquire().expect("ignored SIGTERM permits reacquisition"));
    }

    #[test]
    fn relay_signal_guard_blocks_a_default_sigtstp_instead_of_handling_it() {
        if !run_isolated_signal_probe(
            SIGNAL_OWNER_PROBE,
            "relay_signal_guard_blocks_a_default_sigtstp_instead_of_handling_it",
        ) {
            return;
        }
        assert_eq!(snapshot_action(SIGTSTP).handler, SIG_DFL);
        let original_mask = current_thread_signal_mask().expect("querying the thread signal mask");
        assert!(!signal_in_set(&original_mask, SIGTSTP));

        let guard = RelaySignalGuard::acquire().expect("the relay owns every signal");

        assert!(guard.owns_stop(), "a default unblocked SIGTSTP is eligible");
        assert_eq!(
            snapshot_action(SIGTSTP).handler,
            SIG_DFL,
            "Marion must never install a SIGTSTP action"
        );
        assert!(
            signal_in_set(
                &current_thread_signal_mask().expect("querying the thread signal mask"),
                SIGTSTP
            ),
            "an eligible SIGTSTP stays blocked on the coordinator"
        );
        assert!(!guard.stop_pending().expect("querying pending signals"));
        // SAFETY: SIGTSTP is blocked on this thread, so the raise only marks it pending.
        assert_eq!(unsafe { raise(SIGTSTP) }, 0);
        assert!(
            guard.stop_pending().expect("querying pending signals"),
            "a blocked default SIGTSTP is observed pending, never consumed"
        );
        assert!(
            guard.stop_pending().expect("querying pending signals"),
            "observing the pending stop must not consume it"
        );

        // Discard the pending stop before the mask is restored so this probe is not stopped.
        let _ignored = install_ignored_action(SIGTSTP);
        drop(guard);
        assert_eq!(
            current_thread_signal_mask().expect("querying the thread signal mask"),
            original_mask,
            "restoration returns the exact prior mask"
        );
    }

    #[test]
    fn relay_signal_guard_leaves_an_ignored_sigtstp_untouched() {
        if !run_isolated_signal_probe(
            SIGNAL_OWNER_PROBE,
            "relay_signal_guard_leaves_an_ignored_sigtstp_untouched",
        ) {
            return;
        }
        let _ignored = install_ignored_action(SIGTSTP);
        let original_mask = current_thread_signal_mask().expect("querying the thread signal mask");

        let guard = RelaySignalGuard::acquire().expect("the relay owns every handled signal");

        assert!(
            !guard.owns_stop(),
            "an ignored SIGTSTP is outside Marion ownership"
        );
        assert_eq!(snapshot_action(SIGTSTP).handler, SIG_IGN);
        assert_eq!(
            current_thread_signal_mask().expect("querying the thread signal mask"),
            original_mask,
            "an excluded SIGTSTP keeps its mask membership"
        );
        // SAFETY: the ignored disposition discards the raise.
        assert_eq!(unsafe { raise(SIGTSTP) }, 0);
        assert!(!guard.stop_pending().expect("querying pending signals"));
        drop(guard);
        assert_eq!(snapshot_action(SIGTSTP).handler, SIG_IGN);
    }

    #[test]
    fn relay_signal_guard_excludes_an_already_blocked_sigtstp() {
        if !run_isolated_signal_probe(
            SIGNAL_OWNER_PROBE,
            "relay_signal_guard_excludes_an_already_blocked_sigtstp",
        ) {
            return;
        }
        let _blocked = block_signal_on_current_thread(SIGTSTP);
        let blocked_mask = current_thread_signal_mask().expect("querying the thread signal mask");

        let guard = RelaySignalGuard::acquire().expect("an old-blocked SIGTSTP is not a refusal");

        assert!(!guard.owns_stop());
        assert_eq!(snapshot_action(SIGTSTP).handler, SIG_DFL);
        assert!(!guard.stop_pending().expect("querying pending signals"));
        drop(guard);
        assert_eq!(
            current_thread_signal_mask().expect("querying the thread signal mask"),
            blocked_mask,
            "Marion must not unblock a SIGTSTP it never blocked"
        );
    }

    #[test]
    fn relay_signal_guard_refuses_a_custom_sigtstp_disposition_without_side_effects() {
        if !run_isolated_signal_probe(
            SIGNAL_OWNER_PROBE,
            "relay_signal_guard_refuses_a_custom_sigtstp_disposition_without_side_effects",
        ) {
            return;
        }
        let _restore_originals = install_exact_sentinels();
        let _restore_stop = RestoreActions(vec![(SIGTSTP, snapshot_action(SIGTSTP))]);
        let sentinel = Sigaction {
            handler: sentinel_winch as *const () as usize,
            mask: empty_sigset(),
            flags: 0,
            #[cfg(target_os = "linux")]
            restorer: 0,
        };
        // SAFETY: a valid action for one signal in this isolated probe.
        assert_eq!(
            unsafe { sigaction(SIGTSTP, &sentinel, std::ptr::null_mut()) },
            0
        );
        let actions = snapshot_actions();
        let original_mask = current_thread_signal_mask().expect("querying the thread signal mask");
        RESIZED.store(true, Ordering::SeqCst);
        FIRST_RELAY_SIGNAL.store(SIGINT, Ordering::SeqCst);

        let error = RelaySignalGuard::acquire()
            .err()
            .expect("a custom SIGTSTP disposition refuses relay signal ownership");

        assert!(error.contains("SIGTSTP"), "{error}");
        assert_same_actions(&snapshot_actions(), &actions);
        assert_eq!(snapshot_action(SIGTSTP).handler, sentinel.handler);
        assert_eq!(
            current_thread_signal_mask().expect("querying the thread signal mask"),
            original_mask
        );
        assert!(RESIZED.load(Ordering::SeqCst));
        assert_eq!(FIRST_RELAY_SIGNAL.load(Ordering::SeqCst), SIGINT);
    }

    #[test]
    fn relay_signal_guard_refuses_each_blocked_owned_signal_without_side_effects() {
        if !run_isolated_signal_probe(
            SIGNAL_SYNC_REDELIVERY_PROBE,
            "relay_signal_guard_refuses_each_blocked_owned_signal_without_side_effects",
        ) {
            return;
        }
        let _restore_originals = install_exact_sentinels();
        let actions = snapshot_actions();
        let original_mask = current_thread_signal_mask().expect("querying the thread signal mask");
        for signal in HANDLED_SIGNALS {
            let blocked_mask = block_signal_on_current_thread(signal);
            let expected_blocked_mask = signal_added_to_set(original_mask, signal);
            RESIZED.store(true, Ordering::SeqCst);
            RELAY_SIGNAL_EVENTS.store(0xa5, Ordering::SeqCst);
            FIRST_RELAY_SIGNAL.store(SIGINT, Ordering::SeqCst);
            let mut setup_ran = false;

            let error = setup_after_signal_acquire(|| {
                setup_ran = true;
                Ok(())
            })
            .err()
            .expect("a blocked owned signal prevents relay setup");

            assert!(error.contains("blocked"), "{error}");
            assert!(error.contains(&signal.to_string()), "{error}");
            assert!(!setup_ran, "relay setup ran with signal {signal} blocked");
            assert!(RESIZED.load(Ordering::SeqCst));
            assert_eq!(RELAY_SIGNAL_EVENTS.load(Ordering::SeqCst), 0xa5);
            assert_eq!(FIRST_RELAY_SIGNAL.load(Ordering::SeqCst), SIGINT);
            assert_same_actions(&snapshot_actions(), &actions);
            assert_eq!(
                current_thread_signal_mask().expect("querying the thread signal mask"),
                expected_blocked_mask,
                "relay admission changed the calling thread's mask for signal {signal}"
            );
            drop(blocked_mask);
            assert_eq!(
                current_thread_signal_mask().expect("querying the thread signal mask"),
                original_mask
            );
        }
    }

    #[test]
    fn finish_redelivers_synchronously_while_signal_owner_is_held() {
        if !run_isolated_signal_probe(
            SIGNAL_SYNC_REDELIVERY_PROBE,
            "finish_redelivers_synchronously_while_signal_owner_is_held",
        ) {
            return;
        }
        let _restore_originals = install_exact_sentinels();
        SENTINEL_HITS.store(0, Ordering::SeqCst);
        let mut owner_a = RelaySignalGuard::acquire().expect("relay A owns every signal");
        on_relay_signal(SIGTERM);
        let signal_cleanup = owner_a.restore_result().map_err(|error| error.to_string());
        *BEFORE_REDELIVERY_WHILE_OWNER_HELD
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Box::new(|| {
            let owner_b = RelaySignalGuard::acquire();
            assert!(
                owner_b.is_err(),
                "relay B acquired signal ownership before redelivery"
            );
        }));

        let finish = resolve_relay_finish(
            Ok(RelayStop::ExternalSignal(SIGTERM)),
            Ok(()),
            Ok(()),
            signal_cleanup,
            redeliver_signal,
        );

        assert_eq!(finish, Ok(()));
        assert_eq!(
            SENTINEL_HITS.load(Ordering::SeqCst),
            1,
            "finish returned before the restored sentinel ran"
        );
        drop(owner_a);
        drop(RelaySignalGuard::acquire().expect("relay B reacquires ownership after finish"));
    }

    #[test]
    fn explicit_signal_restore_failure_keeps_drop_fallback_armed() {
        if !run_isolated_signal_probe(
            SIGNAL_OWNER_PROBE,
            "explicit_signal_restore_failure_keeps_drop_fallback_armed",
        ) {
            return;
        }
        let _restore_originals = install_exact_sentinels();
        let sentinels = snapshot_actions();
        let mut guard = RelaySignalGuard::acquire().expect("the relay owns every signal");
        fail_relay_signal_restore_at(1);
        let error = guard
            .restore_result()
            .expect_err("the injected explicit restoration fails");
        assert!(
            error
                .to_string()
                .contains("injected signal action restoration failure"),
            "{error}"
        );
        drop(guard);
        assert_same_actions(&snapshot_actions(), &sentinels);
    }

    #[test]
    fn every_failed_signal_restoration_remains_armed_for_drop_retry() {
        if !run_isolated_signal_probe(
            SIGNAL_OWNER_PROBE,
            "every_failed_signal_restoration_remains_armed_for_drop_retry",
        ) {
            return;
        }
        let _restore_originals = install_exact_sentinels();
        let sentinels = snapshot_actions();
        let mut guard = RelaySignalGuard::acquire().expect("the relay owns every signal");
        fail_relay_signal_restore_at_attempts(&[1, 2]);

        let error = guard
            .restore_result()
            .expect_err("two distinct action restorations fail in one pass");
        assert!(
            error
                .to_string()
                .contains("injected signal action restoration failure"),
            "{error}"
        );

        drop(guard);
        assert_same_actions(&snapshot_actions(), &sentinels);
    }

    #[test]
    fn signal_published_during_action_restoration_is_not_cleared() {
        if !run_isolated_signal_probe(
            SIGNAL_OWNER_PROBE,
            "signal_published_during_action_restoration_is_not_cleared",
        ) {
            return;
        }
        let _restore_originals = install_exact_sentinels();
        let mut guard = RelaySignalGuard::acquire().expect("the relay owns every signal");
        *AFTER_RELAY_SIGNAL_RESTORE
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(Box::new(|| on_relay_signal(SIGTERM)));

        let captured = guard
            .restore_result()
            .expect("restoring the exact prior signal actions");

        assert_eq!(
            captured,
            Some(SIGTERM),
            "a termination published during action restoration was not drained for redelivery"
        );
        assert_eq!(FIRST_RELAY_SIGNAL.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn signal_ownership_is_acquired_before_relay_setup_runs() {
        if !run_isolated_signal_probe(
            SIGNAL_OWNER_PROBE,
            "signal_ownership_is_acquired_before_relay_setup_runs",
        ) {
            return;
        }
        let result = setup_after_signal_acquire(|| {
            let overlap = RelaySignalGuard::acquire()
                .err()
                .expect("setup runs while relay signal ownership is held");
            assert!(overlap.contains("already owns"), "{overlap}");
            Err::<(), _>("injected setup failure".to_string())
        });
        let error = match result {
            Ok(_) => panic!("the injected setup failure unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(error, "injected setup failure");
        drop(RelaySignalGuard::acquire().expect("setup failure finalized signal ownership"));
    }

    #[test]
    fn relay_exit_preserves_the_primary_error_and_names_cleanup_failure() {
        if !run_isolated_signal_probe(
            SIGNAL_OWNER_PROBE,
            "relay_exit_preserves_the_primary_error_and_names_cleanup_failure",
        ) {
            return;
        }
        let RawRelayTerminal {
            master: _master,
            mut terminal,
            observed_stdin,
            baseline_stdin_flags,
        } = raw_relay_terminal_on_test_pty();
        fail_next_stdin_flag_restore();
        let signals = RelaySignalGuard::acquire().expect("the probe owns relay signals");

        let error = finish_claimed_relay(
            &mut terminal,
            Err("reading the native pane socket: reset".into()),
            signals,
        )
        .unwrap_err();

        assert!(
            error.contains("reading the native pane socket: reset"),
            "{error}"
        );
        assert!(error.contains("restoring native terminal state"), "{error}");
        // A failed explicit restore leaves Drop armed for the last-resort retry.
        drop(terminal);
        assert_eq!(
            rustix::fs::fcntl_getfl(&observed_stdin).unwrap(),
            baseline_stdin_flags
        );
    }

    #[test]
    fn production_relay_open_failure_reports_cleanup_and_drop_retries_restoration() {
        if !run_isolated_signal_probe(
            SIGNAL_OWNER_PROBE,
            "production_relay_open_failure_reports_cleanup_and_drop_retries_restoration",
        ) {
            return;
        }
        let RawRelayTerminal {
            master: _master,
            mut terminal,
            observed_stdin,
            baseline_stdin_flags,
        } = raw_relay_terminal_on_test_pty();
        let (client, mut server) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut request = String::new();
            BufReader::new(server.try_clone().unwrap())
                .read_line(&mut request)
                .expect("read native attach request");
            server.write_all(b"not-json\n").unwrap();
            server.flush().unwrap();
        });
        fail_next_stdin_flag_restore();
        let signals = RelaySignalGuard::acquire().expect("the probe owns relay signals");

        let error =
            relay_claimed(AgentId("native".into()), &mut terminal, client, signals).unwrap_err();

        server.join().unwrap();
        assert!(error.contains("unreadable native pane frame"), "{error}");
        assert!(error.contains("terminal restoration"), "{error}");
        drop(terminal);
        assert_eq!(
            rustix::fs::fcntl_getfl(&observed_stdin).unwrap(),
            baseline_stdin_flags
        );
    }

    #[test]
    fn passive_cleanup_bytes_are_attempted_for_complete_and_error_outcomes() {
        let expected = {
            let mut bytes = b"\x1b[?2004l".to_vec();
            bytes.extend_from_slice(&marion_tui::guard::leave_bytes());
            bytes
        };
        for primary in [Ok(RelayStop::Complete), Err("primary failure".to_string())] {
            let mut observed = Vec::new();
            let passive =
                write_passive_terminal_cleanup_to(&mut observed).map_err(|e| e.to_string());
            let _ = resolve_relay_finish(primary, passive, Ok(()), Ok(None), |_| Ok(()));
            assert_eq!(observed, expected);
        }
    }

    #[test]
    fn passive_cleanup_failure_composes_with_the_primary_error() {
        let outcome = resolve_relay_finish(
            Err("primary failure".to_string()),
            Err("cleanup writer failed".to_string()),
            Ok(()),
            Ok(None),
            |_| Ok(()),
        );
        let error = outcome.unwrap_err();
        assert!(error.contains("primary failure"), "{error}");
        assert!(error.contains("passive terminal bytes"), "{error}");
        assert!(error.contains("cleanup writer failed"), "{error}");
    }

    #[test]
    fn returning_signal_redelivery_preserves_a_primary_error() {
        let outcome = resolve_relay_finish(
            Err("primary failure".to_string()),
            Ok(()),
            Ok(()),
            Ok(Some(SIGTERM)),
            |signal| {
                assert_eq!(signal, SIGTERM);
                Ok(())
            },
        );
        let error = outcome.unwrap_err();
        assert_eq!(error, "primary failure");
    }

    #[test]
    fn returning_signal_redelivery_preserves_a_passive_cleanup_error() {
        let outcome = resolve_relay_finish(
            Ok(RelayStop::ExternalSignal(SIGTERM)),
            Err("cleanup writer failed".to_string()),
            Ok(()),
            Ok(Some(SIGTERM)),
            |signal| {
                assert_eq!(signal, SIGTERM);
                Ok(())
            },
        );
        let error = outcome.unwrap_err();
        assert_eq!(
            error,
            "native relay cleanup stage `passive terminal bytes` failed: cleanup writer failed"
        );
    }

    #[test]
    fn returning_signal_redelivery_without_errors_detaches_cleanly() {
        let mut redelivered = None;
        let outcome = resolve_relay_finish(
            Ok(RelayStop::ExternalSignal(SIGTERM)),
            Ok(()),
            Ok(()),
            Ok(Some(SIGTERM)),
            |signal| {
                redelivered = Some(signal);
                Ok(())
            },
        );
        assert_eq!(redelivered, Some(SIGTERM));
        assert_eq!(outcome, Ok(()));
    }

    #[test]
    fn isolated_signal_probe_kills_and_reaps_a_blocked_child_at_its_deadline() {
        if std::env::var_os(SIGNAL_TIMEOUT_PROBE).is_some() {
            std::thread::park();
            unreachable!("the blocked child must be killed by its direct owner");
        }
        let started = Instant::now();
        let error = run_isolated_signal_probe_bounded(
            SIGNAL_TIMEOUT_PROBE,
            "isolated_signal_probe_kills_and_reaps_a_blocked_child_at_its_deadline",
            Duration::from_millis(250),
        )
        .expect_err("the blocked isolated probe reaches its deadline");
        assert!(error.contains("timed out"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the isolated child was not bounded: {:?}",
            started.elapsed()
        );
    }

    fn run_isolated_signal_probe(variable: &str, test: &str) -> bool {
        if std::env::var_os(variable).is_some() {
            return true;
        }
        if let Err(error) =
            run_isolated_signal_probe_bounded(variable, test, Duration::from_secs(5))
        {
            panic!("{error}");
        }
        false
    }

    fn run_isolated_signal_probe_bounded(
        variable: &str,
        test: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        let mut probe = exact_native_relay_test_command(test);
        probe.env(variable, "1");
        let output = crate::run::run_bounded(&mut probe, timeout)
            .map_err(|error| format!("running the bounded isolated probe: {error}"))?;
        if !output.timed_out && output.code == Some(0) {
            Ok(())
        } else {
            let disposition = if output.timed_out {
                format!("timed out after {timeout:?}")
            } else if let Some(code) = output.code {
                format!("exited with code {code}")
            } else if let Some(signal) = output.signal {
                format!("exited from signal {signal}")
            } else {
                "exited without a code or signal".to_string()
            };
            let capture = if output.capture_truncated {
                "output capture was truncated after the bounded drain"
            } else {
                "output capture completed"
            };
            Err(format!(
                "the bounded isolated probe {disposition}; {capture}:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }

    fn exact_native_relay_test_command(test: &str) -> std::process::Command {
        let name = format!(
            "{}::{test}",
            module_path!().split_once("::").expect("crate::module").1
        );
        let mut command = std::process::Command::new(
            std::env::current_exe().expect("the unit-test binary has a path"),
        );
        command.args(["--exact", "--nocapture", "--test-threads", "1", &name]);
        command
    }

    fn fixture_phase(marker: &str) {
        let mut stderr = std::io::stderr().lock();
        writeln!(stderr, "NATIVE_RELAY_FIXTURE_PHASE={marker}")
            .expect("writing the native relay fixture phase");
        stderr
            .flush()
            .expect("flushing the native relay fixture phase");
    }

    fn exit_after_successful_fixture(fixture: fn()) -> ! {
        fixture();
        // Only a successful fixture reaches this point; panics retain libtest's failure capture.
        // SAFETY: the fixture completed and flushed its durable phase markers, while `_exit`
        // prevents unrelated libtest teardown from keeping this isolated runner alive.
        unsafe { _exit(0) }
    }

    fn read_pty_until(
        master: &crate::pty::PtyMaster,
        expected: &[u8],
        deadline: Instant,
    ) -> Result<Vec<u8>, String> {
        let mut observed = Vec::new();
        let mut bytes = [0u8; 1024];
        loop {
            match master.read(&mut bytes) {
                Ok(0) => {
                    return Err(format!(
                        "the pty closed before {:?}; observed {:?}",
                        String::from_utf8_lossy(expected),
                        String::from_utf8_lossy(&observed)
                    ));
                }
                Ok(count) => {
                    observed.extend_from_slice(&bytes[..count]);
                    if observed
                        .windows(expected.len())
                        .any(|window| window == expected)
                    {
                        return Ok(observed);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(format!("reading the relay probe pty: {error}")),
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for {:?}; observed {:?}",
                    String::from_utf8_lossy(expected),
                    String::from_utf8_lossy(&observed)
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn wait_pty_child(
        child: &mut crate::pty::PtyChild,
        deadline: Instant,
    ) -> std::process::ExitStatus {
        loop {
            match child.try_wait().expect("polling the relay probe child") {
                Some(status) => return status,
                None if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                None => {
                    let status = child
                        .kill_and_reap()
                        .expect("killing and reaping the timed-out relay probe");
                    panic!("the relay probe did not exit naturally; killed with {status}");
                }
            }
        }
    }

    fn assert_same_termios(actual: &rustix::termios::Termios, expected: &rustix::termios::Termios) {
        assert_eq!(actual.input_modes, expected.input_modes, "input modes");
        assert_eq!(actual.output_modes, expected.output_modes, "output modes");
        assert_eq!(
            actual.control_modes, expected.control_modes,
            "control modes"
        );
        assert_eq!(
            actual.local_modes & !rustix::termios::LocalModes::PENDIN,
            expected.local_modes,
            "local modes"
        );
        assert_eq!(actual.input_speed(), expected.input_speed(), "input speed");
        assert_eq!(
            actual.output_speed(),
            expected.output_speed(),
            "output speed"
        );
        assert_eq!(
            format!("{:?}", actual.special_codes),
            format!("{:?}", expected.special_codes),
            "special codes"
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            actual.line_discipline, expected.line_discipline,
            "line discipline"
        );
    }

    fn pty_master_termios(master: &crate::pty::PtyMaster) -> rustix::termios::Termios {
        // SAFETY: `PtyMaster` retains this descriptor for the whole borrow and neither transfers
        // nor closes it until `master` drops.
        let master_fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(master.as_raw()) };
        rustix::termios::tcgetattr(master_fd).expect("reading relay probe master termios")
    }

    fn run_native_pty_smoke_inner() {
        fixture_phase("smoke-inner-start");
        let master = crate::pty::PtyMaster::open(crate::pty::WinSize::new(91, 29))
            .expect("opening the relay probe pty");
        // A Darwin PTY master has no queryable terminal attributes until at least one slave is
        // open. Retain this slave to capture the PTY-wide baseline; post-reap observation uses the
        // master because the former controlling-terminal slave becomes ENOTTY after session exit.
        // No file-status flag claim crosses these distinct open file descriptions.
        let observer = master
            .open_slave()
            .expect("opening the relay probe baseline observer");
        let baseline_termios =
            rustix::termios::tcgetattr(&observer).expect("reading relay probe baseline termios");
        let mut command = exact_native_relay_test_command(
            "native_relay_pty_fixture_restores_terminal_after_clean_exit",
        );
        command.env(PTY_SMOKE_PROBE, "1");
        let witness = marion_harness::ExecutionSurfaces::opaque()
            .display_plane()
            .expect("opaque execution owns a pty");
        fixture_phase("smoke-probe-spawn-enter");
        let mut child = crate::pty::spawn_pty(
            witness,
            &mut command,
            &master,
            crate::pty::StdinPlan::TerminalSlave,
            None,
        )
        .expect("spawning the relay probe on its pty");
        fixture_phase("smoke-probe-spawn-complete");
        let deadline = Instant::now() + PTY_SMOKE_LIFECYCLE_BOUND;
        fixture_phase("smoke-output-wait-enter");
        let output = match read_pty_until(&master, b"NATIVE_PTY_DONE\r\n", deadline) {
            Ok(output) => output,
            Err(error) => {
                let cleanup = child.kill_and_reap();
                panic!("{error}; exact child cleanup: {cleanup:?}");
            }
        };
        fixture_phase("smoke-output-wait-complete");
        assert!(
            output
                .windows(b"NATIVE_PTY_READY\n".len())
                .any(|window| window == b"NATIVE_PTY_READY\n"),
            "the probe restored without first entering raw relay mode: {:?}",
            String::from_utf8_lossy(&output)
        );
        fixture_phase("smoke-exit-wait-enter");
        let status = wait_pty_child(&mut child, deadline);
        fixture_phase("smoke-exit-wait-complete");
        assert!(status.success(), "the relay probe exited with {status}");
        let restored_termios = pty_master_termios(&master);
        assert_same_termios(&restored_termios, &baseline_termios);
    }

    fn termios_mismatches(
        actual: &rustix::termios::Termios,
        expected: &rustix::termios::Termios,
    ) -> Vec<String> {
        let mut mismatches = Vec::new();
        if actual.input_modes != expected.input_modes {
            mismatches.push(format!(
                "input_modes: actual={:?}, expected={:?}",
                actual.input_modes, expected.input_modes
            ));
        }
        if actual.output_modes != expected.output_modes {
            mismatches.push(format!(
                "output_modes: actual={:?}, expected={:?}",
                actual.output_modes, expected.output_modes
            ));
        }
        if actual.control_modes != expected.control_modes {
            mismatches.push(format!(
                "control_modes: actual={:?}, expected={:?}",
                actual.control_modes, expected.control_modes
            ));
        }
        let actual_local = actual.local_modes & !rustix::termios::LocalModes::PENDIN;
        if actual_local != expected.local_modes {
            mismatches.push(format!(
                "local_modes: actual={actual_local:?}, expected={:?}",
                expected.local_modes
            ));
        }
        if actual.input_speed() != expected.input_speed() {
            mismatches.push(format!(
                "input_speed: actual={:?}, expected={:?}",
                actual.input_speed(),
                expected.input_speed()
            ));
        }
        if actual.output_speed() != expected.output_speed() {
            mismatches.push(format!(
                "output_speed: actual={:?}, expected={:?}",
                actual.output_speed(),
                expected.output_speed()
            ));
        }
        let actual_codes = format!("{:?}", actual.special_codes);
        let expected_codes = format!("{:?}", expected.special_codes);
        if actual_codes != expected_codes {
            mismatches.push(format!(
                "special_codes: actual={actual_codes}, expected={expected_codes}"
            ));
        }
        #[cfg(target_os = "linux")]
        if actual.line_discipline != expected.line_discipline {
            mismatches.push(format!(
                "line_discipline: actual={:?}, expected={:?}",
                actual.line_discipline, expected.line_discipline
            ));
        }
        mismatches
    }

    fn run_native_pty_sigterm_inner() {
        use std::os::unix::process::ExitStatusExt;

        unsafe extern "C" {
            fn kill(pid: std::ffi::c_int, signal: std::ffi::c_int) -> std::ffi::c_int;
        }

        fixture_phase("sigterm-inner-start");
        let master = crate::pty::PtyMaster::open(crate::pty::WinSize::new(91, 29))
            .expect("opening the SIGTERM relay probe pty");
        let observer = master
            .open_slave()
            .expect("opening the SIGTERM relay baseline observer");
        let baseline_termios =
            rustix::termios::tcgetattr(&observer).expect("reading SIGTERM relay baseline termios");
        let mut command = exact_native_relay_test_command(
            "native_relay_sigterm_restores_terminal_and_redelivers_to_itself",
        );
        command.env(PTY_SIGTERM_PROBE, "1");
        let witness = marion_harness::ExecutionSurfaces::opaque()
            .display_plane()
            .expect("opaque execution owns a pty");
        fixture_phase("sigterm-probe-spawn-enter");
        let mut child = crate::pty::spawn_pty(
            witness,
            &mut command,
            &master,
            crate::pty::StdinPlan::TerminalSlave,
            None,
        )
        .expect("spawning the SIGTERM relay probe on its pty");
        fixture_phase("sigterm-probe-spawn-complete");
        let ready_deadline = Instant::now() + PTY_SIGTERM_READY_BOUND;
        fixture_phase("sigterm-ready-wait-enter");
        if let Err(error) = read_pty_until(&master, b"NATIVE_SIGTERM_READY\n", ready_deadline) {
            let cleanup = child.kill_and_reap();
            panic!("{error}; exact SIGTERM child cleanup: {cleanup:?}");
        }
        fixture_phase("sigterm-ready-wait-complete");

        // SAFETY: `child.pid()` is the exact live `PtyChild` owned here; positive SIGTERM targets
        // only that process, not its group.
        let signal_result = unsafe { kill(child.pid(), SIGTERM) };
        assert_eq!(
            signal_result,
            0,
            "sending SIGTERM to exact relay probe pid {}: {}",
            child.pid(),
            std::io::Error::last_os_error()
        );
        fixture_phase("sigterm-observed-wait-enter");
        if let Err(error) = read_pty_until(
            &master,
            b"NATIVE_SIGTERM_OBSERVED\n",
            Instant::now() + PTY_SIGTERM_OBSERVED_BOUND,
        ) {
            let cleanup = child.kill_and_reap();
            panic!("{error}; exact SIGTERM child cleanup: {cleanup:?}");
        }
        fixture_phase("sigterm-observed-wait-complete");

        let natural_deadline = Instant::now() + PTY_SIGTERM_NATURAL_EXIT_BOUND;
        fixture_phase("sigterm-exit-wait-enter");
        let natural_status = loop {
            match child.try_wait().expect("polling the SIGTERM relay probe") {
                Some(status) => break status,
                None if Instant::now() < natural_deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                None => {
                    // Keep ownership with the bounded outer runner: `kill_and_reap` can itself
                    // block if this nested PTY child is wedged, while the outer process-group
                    // watchdog is independently able to kill the whole fixture tree.
                    std::mem::forget(child);
                    panic!(
                        "the SIGTERM relay probe did not exit naturally within {:?}",
                        PTY_SIGTERM_NATURAL_EXIT_BOUND
                    );
                }
            }
        };
        fixture_phase("sigterm-exit-wait-complete");
        let restored_termios = pty_master_termios(&master);
        let mismatches = termios_mismatches(&restored_termios, &baseline_termios);
        let natural_signal = natural_status.signal();

        assert!(
            natural_signal == Some(SIGTERM) && mismatches.is_empty(),
            "SIGTERM lifecycle mismatch: natural_status={natural_status:?}, natural_signal={natural_signal:?}, termios_mismatches={mismatches:?}"
        );
    }

    #[test]
    fn native_relay_pty_fixture_restores_terminal_after_clean_exit() {
        if std::env::var_os(PTY_SMOKE_PROBE).is_some() {
            let witness = crate::native_tty::capture_process_stdio()
                .expect("the probe owns its foreground controlling terminal");
            let mut terminal = witness
                .enter_native_relay()
                .expect("the probe enters raw relay mode");
            let signals = RelaySignalGuard::acquire().expect("the probe owns relay signals");
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(b"NATIVE_PTY_READY\n").unwrap();
            stdout.flush().unwrap();
            drop(signals);
            terminal
                .restore_result()
                .expect("the probe restores its terminal");
            stdout.write_all(b"NATIVE_PTY_DONE\n").unwrap();
            stdout.flush().unwrap();
            // The probe's observable work is complete. Exit directly so unrelated libtest
            // teardown cannot keep the nested PTY child alive after the terminal is restored.
            // SAFETY: the marker was flushed, no Rust destructor is required by this isolated
            // test process, and `_exit` terminates without running process-global teardown.
            unsafe { _exit(0) }
        }
        if std::env::var_os(PTY_SMOKE_INNER).is_some() {
            exit_after_successful_fixture(run_native_pty_smoke_inner);
        }
        run_isolated_signal_probe_bounded(
            PTY_SMOKE_INNER,
            "native_relay_pty_fixture_restores_terminal_after_clean_exit",
            pty_smoke_outer_bound(),
        )
        .expect("the bounded native relay smoke runner");
    }

    #[test]
    fn native_relay_sigterm_outer_bound_covers_every_legal_inner_phase() {
        let legal_inner_bound =
            PTY_SIGTERM_READY_BOUND + PTY_SIGTERM_OBSERVED_BOUND + PTY_SIGTERM_NATURAL_EXIT_BOUND;
        assert!(
            pty_sigterm_outer_bound() >= PTY_NESTED_RUNNER_STARTUP_BOUND + legal_inner_bound,
            "the outer runner can kill a conforming inner lifecycle: outer={:?}, startup={PTY_NESTED_RUNNER_STARTUP_BOUND:?}, legal inner={legal_inner_bound:?}",
            pty_sigterm_outer_bound()
        );
    }

    #[test]
    fn native_relay_sigterm_restores_terminal_and_redelivers_to_itself() {
        if std::env::var_os(PTY_SIGTERM_PROBE).is_some() {
            let witness = crate::native_tty::capture_process_stdio()
                .expect("the SIGTERM probe owns its foreground controlling terminal");
            let mut terminal = witness
                .enter_native_relay()
                .expect("the SIGTERM probe enters raw relay mode");
            let inherited_sigterm = snapshot_action(SIGTERM);
            assert_eq!(
                inherited_sigterm.handler, SIG_DFL,
                "the SIGTERM probe inherited a non-default disposition: {}",
                inherited_sigterm.handler
            );
            let inherited_mask =
                current_thread_signal_mask().expect("querying the thread signal mask");
            assert!(
                !signal_in_set(&inherited_mask, SIGTERM),
                "the SIGTERM probe inherited SIGTERM blocked: {inherited_mask:?}"
            );
            let signals = RelaySignalGuard::acquire().expect("the probe owns relay signals");
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(b"NATIVE_SIGTERM_READY\n").unwrap();
            stdout.flush().unwrap();
            while FIRST_RELAY_SIGNAL.load(Ordering::SeqCst) == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
            stdout.write_all(b"NATIVE_SIGTERM_OBSERVED\n").unwrap();
            stdout.flush().unwrap();
            drop(stdout);
            let signal = FIRST_RELAY_SIGNAL.load(Ordering::SeqCst);
            finish_claimed_relay(
                &mut terminal,
                Ok(RelayStop::ExternalSignal(signal)),
                signals,
            )
            .expect("the restored prior signal action accepted redelivery");
            return;
        }
        if std::env::var_os(PTY_SIGTERM_INNER).is_some() {
            exit_after_successful_fixture(run_native_pty_sigterm_inner);
        }
        run_isolated_signal_probe_bounded(
            PTY_SIGTERM_INNER,
            "native_relay_sigterm_restores_terminal_and_redelivers_to_itself",
            pty_sigterm_outer_bound(),
        )
        .expect("the bounded SIGTERM lifecycle runner");
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct IdleInput;

    impl Read for IdleInput {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::WouldBlock.into())
        }
    }

    struct FailingOutput;

    impl Write for FailingOutput {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("sentinel terminal write failure"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct OneRead {
        bytes: Option<Vec<u8>>,
        hold: Arc<AtomicBool>,
    }

    impl Read for OneRead {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            if let Some(bytes) = self.bytes.take() {
                output[..bytes.len()].copy_from_slice(&bytes);
                return Ok(bytes.len());
            }
            while !self.hold.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(std::io::ErrorKind::WouldBlock.into())
        }
    }

    fn pane_token() -> marion_proto::PaneReadyTokenV1 {
        marion_proto::PaneReadyTokenV1::new([0x5a; 32])
    }

    fn attach_response() -> Frame {
        use marion_core::encoding::Duration;
        use marion_core::harness::Harness;
        use marion_core::node::{NodeState, ReapState};
        use marion_proto::model::{AttachMode, NodeSummary, ReplayPoint};

        Frame::Response(marion_proto::Response::ok(
            RequestId::Number(1),
            &MethodResult::NodeAttach(marion_proto::result::NodeAttachResult {
                node: NodeSummary {
                    agent_id: AgentId("native".into()),
                    parent_id: None,
                    name: None,
                    agent_type: "synthetic-native".into(),
                    harness: Harness::Codex,
                    harness_version: None,
                    depth: 0,
                    state: NodeState::Running,
                    reap_state: ReapState::Live,
                    timeout: Duration::from_secs(900),
                    pane: true,
                },
                mode: AttachMode::ResubscribeFrom(ReplayPoint {
                    records: 0,
                    src_seq: None,
                }),
                pane: Some(marion_proto::result::PaneAttach {
                    cols: 80,
                    rows: 24,
                    writable: true,
                    held_by: None,
                    pane_ready: Some(marion_proto::result::PaneReadyDescriptorV1 {
                        token: pane_token(),
                        cut: 0,
                    }),
                }),
            }),
        ))
    }

    fn pane_frame(seq: u64, frame: PaneFrameKindV1) -> Frame {
        Frame::Notification(marion_proto::Notification::new(Event::NodePaneFrame(
            marion_proto::PaneFrameV1::new(AgentId("native".into()), seq, frame),
        )))
    }

    fn read_frame(lines: &mut BufReader<UnixStream>) -> Frame {
        let mut line = String::new();
        lines.read_line(&mut line).unwrap();
        Frame::from_line(&line).unwrap()
    }

    fn complete_attach(server: &mut UnixStream) {
        let mut lines = BufReader::new(server.try_clone().unwrap());
        assert!(matches!(read_frame(&mut lines), Frame::Request(_)));
        server
            .write_all(attach_response().to_line().as_bytes())
            .unwrap();
        server.flush().unwrap();
        assert!(matches!(
            read_frame(&mut lines),
            Frame::Input(note) if matches!(note.input, Input::NodeResize { .. })
        ));
        assert!(matches!(
            read_frame(&mut lines),
            Frame::Input(note) if matches!(note.input, Input::NodePaneReady(_))
        ));
    }

    #[derive(Clone, Copy, Debug)]
    enum SignalExit {
        End,
        Detach,
        ReadFailure,
        WriteFailure,
        ResizeFailure,
    }

    type SignalRelay = (
        RawPaneSession<Box<dyn Write>>,
        RelaySignalGuard,
        Option<mpsc::SyncSender<()>>,
        std::thread::JoinHandle<()>,
        Option<std::os::fd::OwnedFd>,
    );

    fn relay_for_signal_exit(exit: SignalExit) -> SignalRelay {
        let (client, mut server) = UnixStream::pair().unwrap();
        let (attached_tx, attached_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let server_thread = std::thread::spawn(move || {
            complete_attach(&mut server);
            attached_tx.send(()).unwrap();
            match exit {
                SignalExit::End => {
                    server
                        .write_all(pane_frame(0, PaneFrameKindV1::End {}).to_line().as_bytes())
                        .unwrap();
                    server.flush().unwrap();
                }
                SignalExit::Detach => {
                    release_rx
                        .recv_timeout(Duration::from_secs(2))
                        .expect("detach returns while the pane socket remains open");
                }
                SignalExit::ReadFailure | SignalExit::ResizeFailure => {}
                SignalExit::WriteFailure => {
                    server
                        .write_all(
                            pane_frame(
                                0,
                                PaneFrameKindV1::Output {
                                    bytes: marion_proto::OpaquePaneBytesV1::new(b"output"),
                                },
                            )
                            .to_line()
                            .as_bytes(),
                        )
                        .unwrap();
                    server.flush().unwrap();
                }
            }
        });

        let input: Box<dyn Read + Send> = match exit {
            SignalExit::Detach => Box::new(std::io::Cursor::new(vec![0x1d, b'd'])),
            _ => Box::new(IdleInput),
        };
        let output: Box<dyn Write> = match exit {
            SignalExit::WriteFailure => Box::new(FailingOutput),
            _ => Box::new(Sink::default()),
        };
        let resize_tty = crate::pty::PtyMaster::open(crate::pty::WinSize::new(80, 24))
            .unwrap()
            .open_slave()
            .unwrap();
        let input_fd = Some(resize_tty.as_raw_fd());
        let signals = RelaySignalGuard::acquire().expect("the relay owns every signal");
        let session = RawPaneSession::open_with_io(
            client,
            AgentId("native".into()),
            input,
            input_fd,
            output,
            None,
        )
        .expect("the signal-owning native relay attaches");
        attached_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the server observed Ready");
        (
            session,
            signals,
            matches!(exit, SignalExit::Detach).then_some(release_tx),
            server_thread,
            Some(resize_tty),
        )
    }

    #[test]
    fn native_relay_restores_the_prior_sigwinch_handler_after_every_exit() {
        if !run_isolated_signal_probe(
            SIGNAL_RESTORE_PROBE,
            "native_relay_restores_the_prior_sigwinch_handler_after_every_exit",
        ) {
            return;
        }
        let _restore_original = install_sentinel();

        for exit in [
            SignalExit::End,
            SignalExit::Detach,
            SignalExit::ReadFailure,
            SignalExit::WriteFailure,
            SignalExit::ResizeFailure,
        ] {
            SENTINEL_HITS.store(0, Ordering::SeqCst);
            RESIZED.store(false, Ordering::SeqCst);
            let (mut session, signals, detach_release, server_thread, _resize_tty) =
                relay_for_signal_exit(exit);

            raise_winch();
            assert!(
                RESIZED.load(Ordering::SeqCst),
                "the relay did not own SIGWINCH during {exit:?}"
            );
            assert_eq!(SENTINEL_HITS.load(Ordering::SeqCst), 0);
            if !matches!(exit, SignalExit::ResizeFailure) {
                RESIZED.store(false, Ordering::SeqCst);
            }

            let outcome = session.pump();
            match exit {
                SignalExit::End | SignalExit::Detach => assert!(outcome.is_ok(), "{outcome:?}"),
                SignalExit::ReadFailure => assert!(
                    outcome.unwrap_err().contains("closed before End"),
                    "wrong read failure"
                ),
                SignalExit::WriteFailure => assert!(
                    outcome.unwrap_err().contains("writing native pane output"),
                    "wrong write failure"
                ),
                SignalExit::ResizeFailure => assert!(
                    outcome.unwrap_err().contains("sending native node/resize"),
                    "wrong resize failure"
                ),
            }
            if let Some(release) = detach_release {
                release.send(()).unwrap();
            }
            drop(session);
            drop(signals);
            server_thread.join().unwrap();

            SENTINEL_HITS.store(0, Ordering::SeqCst);
            RESIZED.store(false, Ordering::SeqCst);
            raise_winch();
            assert_eq!(
                SENTINEL_HITS.load(Ordering::SeqCst),
                1,
                "the exact prior SIGWINCH handler was not restored after {exit:?}"
            );
            assert!(
                !RESIZED.load(Ordering::SeqCst),
                "the relay's handler remained installed after {exit:?}"
            );
        }
    }

    #[test]
    fn native_relay_refuses_overlapping_process_signal_ownership_without_waiting() {
        if !run_isolated_signal_probe(
            SIGNAL_CONTENTION_PROBE,
            "native_relay_refuses_overlapping_process_signal_ownership_without_waiting",
        ) {
            return;
        }
        let _restore_original = install_sentinel();
        let (first, first_signals, first_release, first_server, _first_tty) =
            relay_for_signal_exit(SignalExit::Detach);

        let started = Instant::now();
        let second = RelaySignalGuard::acquire();
        let elapsed = started.elapsed();
        let error = match second {
            Ok(signals) => {
                drop(signals);
                "overlapping relay unexpectedly acquired SIGWINCH".to_string()
            }
            Err(error) => error,
        };

        first_release.unwrap().send(()).unwrap();
        drop(first);
        drop(first_signals);
        first_server.join().unwrap();
        assert!(error.contains("already owns SIGWINCH"), "{error}");
        assert!(
            elapsed < Duration::from_millis(100),
            "signal contention waited for {elapsed:?} instead of refusing"
        );
    }

    #[test]
    fn native_relay_resets_process_resize_state_when_a_session_ends() {
        if !run_isolated_signal_probe(
            SIGNAL_RESET_PROBE,
            "native_relay_resets_process_resize_state_when_a_session_ends",
        ) {
            return;
        }
        let _restore_original = install_sentinel();
        RESIZED.store(false, Ordering::SeqCst);
        let (session, signals, release, server_thread, _resize_tty) =
            relay_for_signal_exit(SignalExit::Detach);
        RESIZED.store(true, Ordering::SeqCst);
        release.unwrap().send(()).unwrap();
        drop(session);
        drop(signals);
        server_thread.join().unwrap();

        assert!(
            !RESIZED.load(Ordering::SeqCst),
            "a later native relay would inherit the prior session's resize edge"
        );
    }

    #[test]
    fn resize_arriving_before_signal_acquisition_survives_session_initialization() {
        if !run_isolated_signal_probe(
            SIGNAL_RESET_PROBE,
            "resize_arriving_before_signal_acquisition_survives_session_initialization",
        ) {
            return;
        }
        let _restore_original = install_sentinel();
        let resize_tty = crate::pty::PtyMaster::open(crate::pty::WinSize::new(80, 24)).unwrap();
        let input = resize_tty.open_slave().unwrap();
        let input_fd = input.as_raw_fd();
        let (client, mut server) = UnixStream::pair().unwrap();
        *AFTER_RESIZE_SIGNAL_ACQUIRE
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Box::new(move || {
            resize_tty
                .set_size(crate::pty::WinSize::new(132, 47))
                .unwrap();
            raise_winch();
        }));
        let server_thread = std::thread::spawn(move || {
            let mut lines = BufReader::new(server.try_clone().unwrap());
            assert!(matches!(read_frame(&mut lines), Frame::Request(_)));
            server
                .write_all(attach_response().to_line().as_bytes())
                .unwrap();
            server.flush().unwrap();
            let Frame::Input(note) = read_frame(&mut lines) else {
                panic!("native relay omitted initial geometry")
            };
            assert!(
                matches!(
                    note.input,
                    Input::NodeResize {
                        cols: 132,
                        rows: 47,
                        ..
                    }
                ),
                "resize in the geometry-to-handler seam was lost: {:?}",
                note.input
            );
            assert!(matches!(read_frame(&mut lines), Frame::Input(_)));
        });

        let signals = RelaySignalGuard::acquire().expect("the relay owns every signal");
        let session = RawPaneSession::open_with_io(
            client,
            AgentId("native".into()),
            IdleInput,
            Some(input_fd),
            Sink::default(),
            None,
        )
        .expect("the native relay attaches");
        assert!(
            RESIZED.load(Ordering::SeqCst),
            "the resize edge arriving after handler installation was erased"
        );
        drop(session);
        drop(signals);
        server_thread.join().unwrap();
    }

    #[test]
    fn raw_pane_v1_writes_invalid_utf8_tail_verbatim_without_a_screen_preamble() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let server_thread = std::thread::spawn(move || {
            let mut lines = BufReader::new(server.try_clone().unwrap());
            let Frame::Request(request) = read_frame(&mut lines) else {
                panic!("relay did not attach")
            };
            assert!(matches!(request.call, Call::NodeAttach(_)));
            server
                .write_all(attach_response().to_line().as_bytes())
                .unwrap();
            server.flush().unwrap();

            assert!(matches!(
                read_frame(&mut lines),
                Frame::Input(note) if matches!(note.input, marion_proto::Input::NodeResize { cols: 111, rows: 37, .. })
            ));
            assert!(matches!(
                read_frame(&mut lines),
                Frame::Input(note) if matches!(note.input, marion_proto::Input::NodePaneReady(_))
            ));

            let tail = [0xff, 0x00, 0x9b, b'3', b'1', b'm', b'Z'];
            server
                .write_all(
                    pane_frame(
                        0,
                        PaneFrameKindV1::Output {
                            bytes: marion_proto::OpaquePaneBytesV1::new(tail),
                        },
                    )
                    .to_line()
                    .as_bytes(),
                )
                .unwrap();
            server
                .write_all(
                    pane_frame(
                        1,
                        PaneFrameKindV1::Resize {
                            cols: 111,
                            rows: 37,
                        },
                    )
                    .to_line()
                    .as_bytes(),
                )
                .unwrap();
            server
                .write_all(pane_frame(2, PaneFrameKindV1::End {}).to_line().as_bytes())
                .unwrap();
            server.flush().unwrap();
        });

        let sink = Sink::default();
        let mut session = RawPaneSession::open_for_test(
            client,
            AgentId("native".into()),
            IdleInput,
            sink.clone(),
            (111, 37),
        )
        .expect("the pane-v1 relay attaches");
        session.pump().expect("the relay reaches End");
        server_thread.join().unwrap();

        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            &[0xff, 0x00, 0x9b, b'3', b'1', b'm', b'Z']
        );
    }

    #[test]
    fn raw_pane_v1_forwards_invalid_utf8_keyboard_bytes_without_text_conversion() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let hold = Arc::new(AtomicBool::new(false));
        let release = Arc::clone(&hold);
        let server_thread = std::thread::spawn(move || {
            let mut lines = BufReader::new(server.try_clone().unwrap());
            assert!(matches!(read_frame(&mut lines), Frame::Request(_)));
            server
                .write_all(attach_response().to_line().as_bytes())
                .unwrap();
            server.flush().unwrap();
            assert!(matches!(read_frame(&mut lines), Frame::Input(_))); // initial Resize
            assert!(matches!(read_frame(&mut lines), Frame::Input(_))); // Ready
            let Frame::Input(note) = read_frame(&mut lines) else {
                panic!("native keyboard did not send an input notification")
            };
            let Input::NodePaneWrite(write) = note.input else {
                panic!("native keyboard used a lossy legacy input method")
            };
            assert_eq!(write.bytes.as_bytes(), &[0xff, 0x00, 0x80, b'X']);
            server
                .write_all(pane_frame(0, PaneFrameKindV1::End {}).to_line().as_bytes())
                .unwrap();
            server.flush().unwrap();
            release.store(true, Ordering::SeqCst);
        });

        let mut session = RawPaneSession::open_for_test(
            client,
            AgentId("native".into()),
            OneRead {
                bytes: Some(vec![0xff, 0x00, 0x80, b'X']),
                hold,
            },
            Sink::default(),
            (80, 24),
        )
        .unwrap();
        session.pump().unwrap();
        server_thread.join().unwrap();
    }

    #[test]
    fn raw_pane_v1_rejects_a_frame_coalesced_before_ready() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let server_thread = std::thread::spawn(move || {
            let mut lines = BufReader::new(server.try_clone().unwrap());
            assert!(matches!(read_frame(&mut lines), Frame::Request(_)));
            let mut bytes = attach_response().to_line();
            bytes.push_str(&pane_frame(0, PaneFrameKindV1::End {}).to_line());
            server.write_all(bytes.as_bytes()).unwrap();
            server.flush().unwrap();
            let _ = read_frame(&mut lines);
        });

        let error = match RawPaneSession::open_for_test(
            client,
            AgentId("native".into()),
            IdleInput,
            Sink::default(),
            (80, 24),
        ) {
            Ok(_) => panic!("a pane frame before Ready must fail closed"),
            Err(error) => error,
        };
        assert!(error.contains("before native Ready"), "{error}");
        server_thread.join().unwrap();
    }

    #[test]
    fn raw_pane_v1_rejects_sequence_gap_end_before_cut_and_socket_loss() {
        enum Failure {
            Gap,
            EarlyEnd,
            Disconnect,
        }

        for (failure, expected) in [
            (Failure::Gap, "not dense"),
            (Failure::EarlyEnd, "before the advertised replay cut"),
            (Failure::Disconnect, "closed before End"),
        ] {
            let (client, mut server) = UnixStream::pair().unwrap();
            let server_thread = std::thread::spawn(move || {
                let mut lines = BufReader::new(server.try_clone().unwrap());
                assert!(matches!(read_frame(&mut lines), Frame::Request(_)));
                let mut response = attach_response();
                if matches!(failure, Failure::EarlyEnd) {
                    let Frame::Response(ref mut response) = response else {
                        unreachable!()
                    };
                    let MethodResult::NodeAttach(mut attached) = marion_proto::Method::NodeAttach
                        .decode_result(match &response.outcome {
                            marion_proto::Outcome::Result(body) => body,
                            _ => unreachable!(),
                        })
                        .unwrap()
                    else {
                        unreachable!()
                    };
                    attached
                        .pane
                        .as_mut()
                        .unwrap()
                        .pane_ready
                        .as_mut()
                        .unwrap()
                        .cut = 4;
                    *response = marion_proto::Response::ok(
                        RequestId::Number(1),
                        &MethodResult::NodeAttach(attached),
                    );
                }
                server.write_all(response.to_line().as_bytes()).unwrap();
                server.flush().unwrap();
                let _ = read_frame(&mut lines);
                let _ = read_frame(&mut lines);
                match failure {
                    Failure::Gap => server
                        .write_all(pane_frame(2, PaneFrameKindV1::End {}).to_line().as_bytes())
                        .unwrap(),
                    Failure::EarlyEnd => server
                        .write_all(pane_frame(0, PaneFrameKindV1::End {}).to_line().as_bytes())
                        .unwrap(),
                    Failure::Disconnect => return,
                }
                server.flush().unwrap();
            });
            let mut session = RawPaneSession::open_for_test(
                client,
                AgentId("native".into()),
                IdleInput,
                Sink::default(),
                (80, 24),
            )
            .unwrap();
            let error = session.pump().unwrap_err();
            assert!(error.contains(expected), "{error}");
            server_thread.join().unwrap();
        }
    }
}
