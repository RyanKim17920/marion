//! Tests for `native_relay.rs`, moved out of it unchanged.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_core::proto::{Call, Event, Frame, Input, MethodResult, PaneFrameKindV1, RequestId};

use super::{
    AFTER_KEYBOARD_INPUT_WRITE, AFTER_RELAY_SIGNAL_RESTORE, AFTER_RESIZE_SIGNAL_ACQUIRE,
    BEFORE_REDELIVERY_WHILE_OWNER_HELD, BorrowedTerminalWriter, FIRST_RELAY_SIGNAL,
    HANDLED_SIGNALS, HUNG_UP, INTERRUPTED, RELAY_SIGNAL_EVENTS, RESIZED, RawPaneSession,
    RelaySignalGuard, RelayStop, SIG_BLOCK, SIG_DFL, SIG_IGN, SIGHUP, SIGINT, SIGTERM, SIGTSTP,
    SIGWINCH, SigSet, Sigaction, TERMINATED, WHILE_STOPPED_WITH_TERMINATIONS_BLOCKED,
    current_thread_signal_mask, empty_sigset, fail_relay_signal_install_at,
    fail_relay_signal_restore_at, fail_relay_signal_restore_at_attempts, finish_claimed_relay,
    on_relay_signal, owned_stop_pending, pthread_sigmask, raise, redeliver_signal, relay_claimed,
    resolve_relay_finish, setup_after_signal_acquire, sigaction, signal, signal_in_set, signal_set,
    status_clear_bytes, status_line, status_overlay_bytes, suspend_until_continued,
    write_passive_terminal_cleanup_to,
};
use crate::native_tty::test_support::{
    RawRelayTerminal, fail_next_stdin_flag_restore, queue_status_flag_results,
    raw_relay_terminal_on_test_pty,
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
const PTY_STOP_INNER: &str = "MARION_NATIVE_PTY_STOP_INNER";
const PTY_STOP_LEADER: &str = "MARION_NATIVE_PTY_STOP_LEADER";
/// Each of the leader's four observed phases: raw entry, stop, raw re-entry, exit.
const PTY_STOP_PHASE_BOUND: Duration = Duration::from_secs(3);
const PTY_STOP_INNER_BOUND: Duration = Duration::from_secs(15);
/// How long a held keyboard worker waits for the pump to reach its verdict before giving up:
/// a broken ordering must fail the assertion, never hang the harness.
const PANE_VERDICT_BOUND: Duration = Duration::from_secs(5);
const SIGTTOU: std::ffi::c_int = 22;
const SIG_ERR: usize = usize::MAX;
static SENTINEL_HITS: AtomicUsize = AtomicUsize::new(0);

#[cfg(target_os = "macos")]
const TEST_SA_RESTART: i32 = 0x0002;
#[cfg(target_os = "linux")]
const TEST_SA_RESTART: i32 = 0x1000_0000;

/// The flag libc sets on a Linux disposition when it hands the kernel a signal-return
/// trampoline of its own. Where it is present `sa_restorer` is a field of the installed
/// disposition and a query reports it; where it is absent — aarch64, whose kernel has no such
/// field — a query reports whatever libc's own buffer happened to hold.
#[cfg(target_os = "linux")]
const TEST_SA_RESTORER: i32 = 0x0400_0000;

fn pty_smoke_outer_bound() -> Duration {
    PTY_NESTED_RUNNER_STARTUP_BOUND + PTY_SMOKE_LIFECYCLE_BOUND
}

fn pty_stop_outer_bound() -> Duration {
    PTY_NESTED_RUNNER_STARTUP_BOUND + PTY_STOP_INNER_BOUND
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

/// The exact-action oracle: what libc reports for the sentinel actions once each has been
/// installed and read back the way [`restore_signal_action`] installs and [`snapshot_actions`]
/// reads.
///
/// Restoration hands libc back the captured `struct sigaction` byte for byte, so handler,
/// mask and flags survive a relay exactly. `sa_restorer` does not, and cannot: it exists only
/// on Linux and it is libc's field, not marion's. glibc fills the kernel struct's restorer
/// from its own trampoline on installation, so what a query reports afterwards is glibc's
/// choice for that call rather than the pointer the caller passed — an install carrying a
/// deliberately different restorer changes nothing in the next query on x86_64, and on
/// aarch64, where the kernel has no such field to fill, a query hands back libc's own leftover.
/// Comparing a snapshot taken before any restoration against one taken after would therefore
/// compare two values libc owns on opposite sides of its bookkeeping, which no verbatim
/// restore can make equal. Reading the oracle back through one verbatim install of the very
/// struct restoration replays puts both sides of [`assert_same_actions`] on the same side of
/// that bookkeeping: the fields marion owns stay exact, and libc's field is held to what libc
/// itself reports for this struct.
fn restored_action_oracle() -> Vec<Sigaction> {
    let captured = snapshot_actions();
    for (signal, action) in HANDLED_SIGNALS.iter().zip(captured.iter()) {
        // SAFETY: each action is the unmodified value libc just reported for this signal, and
        // the isolated probe has no concurrent signal owner.
        assert_eq!(
            unsafe { sigaction(*signal, action, std::ptr::null_mut()) },
            0
        );
    }
    snapshot_actions()
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

/// The part of a queried `sa_mask` the platform actually defines.
///
/// Darwin's `sigset_t` is one word and all of it is the mask. Linux's is 128 bytes wide inside
/// libc's `struct sigaction`, but `rt_sigaction` carries only `_NSIG / 8` of them — one word,
/// every signal the kernel has — and libc copies the remainder of its own uninitialized buffer
/// out alongside them. Those trailing bytes are not a disposition: they are whatever libc's
/// frame last held, and they move when an unrelated call runs between two queries. Comparing
/// them would assert on uninitialized memory, so the oracle stops where the kernel does.
#[cfg(target_os = "macos")]
fn defined_mask(mask: &SigSet) -> SigSet {
    *mask
}

#[cfg(target_os = "linux")]
fn defined_mask(mask: &SigSet) -> usize {
    mask[0]
}

fn assert_same_actions(actual: &[Sigaction], expected: &[Sigaction]) {
    assert_eq!(actual.len(), expected.len());
    for (signal, (actual, expected)) in HANDLED_SIGNALS
        .iter()
        .zip(actual.iter().zip(expected.iter()))
    {
        assert_eq!(actual.handler, expected.handler, "handler for {signal}");
        assert_eq!(
            defined_mask(&actual.mask),
            defined_mask(&expected.mask),
            "mask for {signal}"
        );
        assert_eq!(actual.flags, expected.flags, "flags for {signal}");
        // `sa_restorer` is libc's field, not marion's, and it is only a field of the
        // disposition at all where libc says so. Where it is, restoration must hand back the
        // captured pointer and a query must report it; where it is not, the value a query
        // yields belongs to no disposition and asserting on it asserts on libc's leftovers.
        #[cfg(target_os = "linux")]
        if expected.flags & TEST_SA_RESTORER != 0 {
            assert_eq!(actual.restorer, expected.restorer, "restorer for {signal}");
        }
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
    let sentinels = restored_action_oracle();
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
    let sentinels = restored_action_oracle();
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
    assert!(!owned_stop_pending().expect("querying pending signals"));
    // SAFETY: SIGTSTP is blocked on this thread, so the raise only marks it pending.
    assert_eq!(unsafe { raise(SIGTSTP) }, 0);
    assert!(
        owned_stop_pending().expect("querying pending signals"),
        "a blocked default SIGTSTP is observed pending, never consumed"
    );
    assert!(
        owned_stop_pending().expect("querying pending signals"),
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
    assert!(!owned_stop_pending().expect("querying pending signals"));
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
    assert!(!owned_stop_pending().expect("querying pending signals"));
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
    let sentinels = restored_action_oracle();
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
    let sentinels = restored_action_oracle();
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
        .unwrap_or_else(|error| error.into_inner()) = Some(Box::new(|| on_relay_signal(SIGTERM)));

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
        // Bound, not swallowed by `..`: a field a pattern does not name is dropped at the
        // destructuring, and dropping the master closes the last master descriptor. Linux then
        // hangs the slaves up, so the terminal this probe is holding stops being a terminal
        // mid-test. Darwin keeps answering, which is why `..` survived here.
        master: _master,
        mut terminal,
        observed_stdin,
        baseline_stdin_flags,
        ..
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
        // Bound, not swallowed by `..`: a field a pattern does not name is dropped at the
        // destructuring, and dropping the master closes the last master descriptor. Linux then
        // hangs the slaves up, so the terminal this probe is holding stops being a terminal
        // mid-test. Darwin keeps answering, which is why `..` survived here.
        master: _master,
        mut terminal,
        observed_stdin,
        baseline_stdin_flags,
        ..
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
        let passive = write_passive_terminal_cleanup_to(&mut observed).map_err(|e| e.to_string());
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
    if let Err(error) = run_isolated_signal_probe_bounded(variable, test, Duration::from_secs(5)) {
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

/// Read whatever the probe has already written without waiting for more.
///
/// A terminal never stops reading, and neither may a PTY fixture that is waiting for its
/// child to exit: the probe is the session leader of its controlling PTY, and `proc_exit`
/// drains a session leader's controlling terminal before the process becomes reapable. Bytes
/// written after the last marker the fixture read (the passive cleanup sequence) would
/// otherwise pin the exiting probe until the master reads them, which no bounded `try_wait`
/// can observe.
fn drain_pty(master: &crate::pty::PtyMaster, drained: &mut Vec<u8>) {
    let mut bytes = [0u8; 1024];
    loop {
        match master.read(&mut bytes) {
            Ok(0) => return,
            Ok(count) => drained.extend_from_slice(&bytes[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(error) => panic!("draining the relay probe pty: {error}"),
        }
    }
}

fn wait_pty_child(
    master: &crate::pty::PtyMaster,
    child: &mut crate::pty::PtyChild,
    deadline: Instant,
) -> std::process::ExitStatus {
    let mut drained = Vec::new();
    loop {
        drain_pty(master, &mut drained);
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
    let status = wait_pty_child(&master, &mut child, deadline);
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
    let mut output = match read_pty_until(
        &master,
        b"NATIVE_SIGTERM_OBSERVED\n",
        Instant::now() + PTY_SIGTERM_OBSERVED_BOUND,
    ) {
        Ok(output) => output,
        Err(error) => {
            let cleanup = child.kill_and_reap();
            panic!("{error}; exact SIGTERM child cleanup: {cleanup:?}");
        }
    };
    fixture_phase("sigterm-observed-wait-complete");

    let natural_deadline = Instant::now() + PTY_SIGTERM_NATURAL_EXIT_BOUND;
    fixture_phase("sigterm-exit-wait-enter");
    let natural_status = loop {
        drain_pty(&master, &mut output);
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
    let mut passive_cleanup = Vec::new();
    write_passive_terminal_cleanup_to(&mut passive_cleanup)
        .expect("rendering the expected passive cleanup bytes");
    let cleanup_written = output
        .windows(passive_cleanup.len())
        .any(|window| window == passive_cleanup);

    assert!(
        natural_signal == Some(SIGTERM) && mismatches.is_empty() && cleanup_written,
        "SIGTERM lifecycle mismatch: natural_status={natural_status:?}, natural_signal={natural_signal:?}, termios_mismatches={mismatches:?}, passive_cleanup_written={cleanup_written}, output={:?}",
        String::from_utf8_lossy(&output)
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
        let inherited_mask = current_thread_signal_mask().expect("querying the thread signal mask");
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

/// One job-control child owned by the leader fixture: the probe as a foreground job.
///
/// The probe is a `fork` of the leader rather than a re-exec of the test binary: a libtest
/// process always has its harness main thread, whose mask does not block `SIGTSTP`, and a
/// process-directed stop is delivered to any thread that does not block it. The relay owns the
/// stop by blocking it on the relay thread and letting later threads inherit that mask, which
/// holds for the shipped binary (the relay runs on the only thread) and for a forked child
/// whose sole thread is the relay thread. `std::process::Child::try_wait` never passes
/// `WUNTRACED`, so every observation here goes through one `waitpid` that does, and the exit
/// status is recorded so the pid is signalled only while this owner still holds it unreaped.
struct ForegroundJob {
    pid: rustix::process::Pid,
    reaped: Option<rustix::process::WaitStatus>,
}

impl ForegroundJob {
    fn fork(body: fn()) -> Self {
        unsafe extern "C" {
            fn fork() -> std::ffi::c_int;
            fn setpgid(pid: std::ffi::c_int, pgid: std::ffi::c_int) -> std::ffi::c_int;
            fn tcsetpgrp(fd: std::ffi::c_int, pgrp: std::ffi::c_int) -> std::ffi::c_int;
            fn getpid() -> std::ffi::c_int;
        }
        // SAFETY: the child is a fresh single-threaded process; the parent's other threads are
        // parked in the libtest harness, not inside an allocator or I/O lock.
        let pid = unsafe { fork() };
        assert!(
            pid >= 0,
            "forking the foreground stop probe: {}",
            std::io::Error::last_os_error()
        );
        if pid == 0 {
            let outcome = std::panic::catch_unwind(|| {
                // A new process group in the leader's session is not orphaned, so the kernel
                // honours a default stop for it. Claiming the foreground from a background
                // group raises `SIGTTOU`, so that signal is blocked for the one syscall and
                // the probe body starts with an untouched mask.
                // SAFETY: plain syscalls on this process and its inherited controlling tty.
                unsafe {
                    assert_eq!(setpgid(0, 0), 0, "{}", std::io::Error::last_os_error());
                    let ttou = signal_set(SIGTTOU);
                    assert_eq!(pthread_sigmask(SIG_BLOCK, &ttou, std::ptr::null_mut()), 0);
                    assert_eq!(
                        tcsetpgrp(0, getpid()),
                        0,
                        "{}",
                        std::io::Error::last_os_error()
                    );
                    assert_eq!(
                        pthread_sigmask(super::SIG_UNBLOCK, &ttou, std::ptr::null_mut()),
                        0
                    );
                }
                body();
            });
            // The body ends by dying from its redelivered signal; reaching here is a failure
            // the leader observes as a plain exit. `_exit` keeps the forked copy of the
            // libtest harness from ever running.
            // SAFETY: nothing in this forked child needs destructors.
            unsafe { _exit(if outcome.is_ok() { 0 } else { 101 }) }
        }
        Self {
            pid: rustix::process::Pid::from_raw(pid).expect("a positive child pid"),
            reaped: None,
        }
    }

    fn signal_group(&self, signal: rustix::process::Signal) {
        assert!(
            self.reaped.is_none(),
            "signalling a reaped foreground job would race pid reuse"
        );
        rustix::process::kill_process_group(self.pid, signal)
            .expect("signalling the foreground job's process group");
    }

    fn signal(&self, signal: rustix::process::Signal) {
        assert!(
            self.reaped.is_none(),
            "signalling a reaped foreground job would race pid reuse"
        );
        rustix::process::kill_process(self.pid, signal).expect("signalling the foreground job");
    }

    /// One non-blocking `waitpid(WUNTRACED)`: a stop, a continue, or an exit, if any.
    fn poll(&mut self) -> Option<rustix::process::WaitStatus> {
        use rustix::process::{WaitOptions, waitpid};
        if let Some(status) = self.reaped {
            return Some(status);
        }
        let status = waitpid(Some(self.pid), WaitOptions::NOHANG | WaitOptions::UNTRACED)
            .expect("polling the foreground stop probe")
            .map(|(_, status)| status);
        if let Some(status) = status
            && (status.exited() || status.signaled())
        {
            self.reaped = Some(status);
        }
        status
    }

    /// Poll until `accept` names the status it wants or the bound expires.
    fn wait_until(
        &mut self,
        bound: Duration,
        accept: impl Fn(rustix::process::WaitStatus) -> bool,
    ) -> Result<rustix::process::WaitStatus, String> {
        let deadline = Instant::now() + bound;
        loop {
            if let Some(status) = self.poll()
                && accept(status)
            {
                return Ok(status);
            }
            if let Some(status) = self.reaped {
                return Err(format!("the foreground job exited early: {status:?}"));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "the foreground job did not reach the expected state within {bound:?}"
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Drop for ForegroundJob {
    fn drop(&mut self) {
        if self.reaped.is_some() {
            return;
        }
        // `SIGKILL` ends a stopped job too; the wait after it is the one blocking call and is
        // bounded by the kill itself.
        let _ = rustix::process::kill_process(self.pid, rustix::process::Signal::KILL);
        self.reaped =
            rustix::process::waitpid(Some(self.pid), rustix::process::WaitOptions::empty())
                .ok()
                .flatten()
                .map(|(_, status)| status);
    }
}

fn cooked_termios(fd: impl std::os::fd::AsFd) -> rustix::termios::Termios {
    rustix::termios::tcgetattr(fd).expect("reading the job-control terminal")
}

fn is_raw(termios: &rustix::termios::Termios) -> bool {
    !termios
        .local_modes
        .contains(rustix::termios::LocalModes::ICANON)
}

/// The job-control leader: session leader on the fixture PTY, running the probe as its
/// foreground job. It never reads the terminal (the inner fixture drains the master), so its
/// oracles are `waitpid(WUNTRACED)` and the terminal's own attributes.
fn run_native_pty_stop_leader() {
    let stdin = std::io::stdin();
    let baseline = cooked_termios(&stdin);
    assert!(!is_raw(&baseline), "the leader's terminal starts cooked");
    let mut job = ForegroundJob::fork(run_native_pty_stop_probe);

    // Raw mode is the probe's ready oracle: it acquires signal ownership (blocking SIGTSTP)
    // before entering raw mode, so a stop sent now is owned rather than kernel-default.
    let raw_deadline = Instant::now() + PTY_STOP_PHASE_BOUND;
    while !is_raw(&cooked_termios(&stdin)) {
        if let Some(status) = job.poll() {
            panic!("the stop probe ended before entering raw mode: {status:?}");
        }
        assert!(
            Instant::now() < raw_deadline,
            "the stop probe never entered raw mode"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    job.signal_group(rustix::process::Signal::TSTP);
    let stopped = job
        .wait_until(PTY_STOP_PHASE_BOUND, |status| status.stopped())
        .expect("the kernel reported the revealed stop");
    assert_eq!(
        stopped.stopping_signal(),
        Some(SIGTSTP),
        "the job stopped for a different signal: {stopped:?}"
    );
    let while_stopped = termios_mismatches(&cooked_termios(&stdin), &baseline);
    assert!(
        while_stopped.is_empty(),
        "the terminal was not cooked while the job was stopped: {while_stopped:?}"
    );
    println!("LEADER_STOPPED_COOKED");

    job.signal_group(rustix::process::Signal::CONT);
    let reraw_deadline = Instant::now() + PTY_STOP_PHASE_BOUND;
    while !is_raw(&cooked_termios(&stdin)) {
        if let Some(status) = job.poll()
            && (status.exited() || status.signaled())
        {
            panic!("the stop probe ended before re-entering raw mode: {status:?}");
        }
        assert!(
            Instant::now() < reraw_deadline,
            "the stop probe never re-entered raw mode after SIGCONT"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    println!("LEADER_RESUMED_RAW");

    job.signal(rustix::process::Signal::TERM);
    let ended = job
        .wait_until(PTY_STOP_PHASE_BOUND, |status| status.signaled())
        .expect("the probe redelivered SIGTERM to itself");
    assert_eq!(
        ended.terminating_signal(),
        Some(SIGTERM),
        "the probe did not end by SIGTERM: {ended:?}"
    );
    let after_exit = termios_mismatches(&cooked_termios(&stdin), &baseline);
    assert!(
        after_exit.is_empty(),
        "the terminal was not restored after the probe ended: {after_exit:?}"
    );
    println!("LEADER_EXITED_COOKED");
}

fn run_native_pty_stop_inner() {
    fixture_phase("stop-inner-start");
    let master = crate::pty::PtyMaster::open(crate::pty::WinSize::new(91, 29))
        .expect("opening the stop relay probe pty");
    let observer = master
        .open_slave()
        .expect("opening the stop relay baseline observer");
    let baseline_termios =
        rustix::termios::tcgetattr(&observer).expect("reading stop relay baseline termios");
    let mut command = exact_native_relay_test_command(
        "native_relay_stop_and_continue_are_observed_by_a_job_control_leader",
    );
    command.env(PTY_STOP_LEADER, "1");
    let witness = marion_harness::ExecutionSurfaces::opaque()
        .display_plane()
        .expect("opaque execution owns a pty");
    fixture_phase("stop-leader-spawn-enter");
    let mut leader = crate::pty::spawn_pty(
        witness,
        &mut command,
        &master,
        crate::pty::StdinPlan::TerminalSlave,
        None,
    )
    .expect("spawning the job-control leader on its pty");
    fixture_phase("stop-leader-spawn-complete");
    let deadline = Instant::now() + PTY_STOP_INNER_BOUND;
    fixture_phase("stop-leader-wait-enter");
    let mut output = match read_pty_until(&master, b"LEADER_EXITED_COOKED\r\n", deadline) {
        Ok(output) => output,
        Err(error) => {
            let cleanup = leader.kill_and_reap();
            panic!("{error}; exact leader cleanup: {cleanup:?}");
        }
    };
    fixture_phase("stop-leader-wait-complete");
    let status = wait_pty_child(&master, &mut leader, deadline);
    drain_pty(&master, &mut output);
    assert!(
        status.success(),
        "the job-control leader exited with {status}"
    );

    let mut passive_cleanup = Vec::new();
    write_passive_terminal_cleanup_to(&mut passive_cleanup)
        .expect("rendering the expected passive cleanup bytes");
    let position = |needle: &[u8]| {
        output
            .windows(needle.len())
            .position(|window| window == needle)
            .unwrap_or_else(|| {
                panic!(
                    "{:?} never crossed the pty: {:?}",
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&output)
                )
            })
    };
    // Markers written in raw mode cross without `ONLCR`, so the line endings are not matched.
    let ready = position(b"NATIVE_STOP_READY");
    let cleanup = position(&passive_cleanup);
    let stopped = position(b"LEADER_STOPPED_COOKED");
    let resumed = position(b"NATIVE_STOP_RESUMED");
    let resumed_raw = position(b"LEADER_RESUMED_RAW");
    assert!(
        ready < cleanup && cleanup < stopped && stopped < resumed && stopped < resumed_raw,
        "the stop lifecycle crossed the pty out of order: ready={ready}, cleanup={cleanup}, stopped={stopped}, resumed={resumed}, resumed_raw={resumed_raw}, output={:?}",
        String::from_utf8_lossy(&output)
    );
    let restored = termios_mismatches(&pty_master_termios(&master), &baseline_termios);
    assert!(
        restored.is_empty(),
        "the fixture pty was left modified: {restored:?}"
    );
}

#[test]
fn native_relay_stop_outer_bound_covers_every_legal_inner_phase() {
    assert!(
        PTY_STOP_INNER_BOUND >= PTY_NESTED_RUNNER_STARTUP_BOUND + 4 * PTY_STOP_PHASE_BOUND,
        "the inner runner can outlive a conforming leader: inner={PTY_STOP_INNER_BOUND:?}, leader phases={:?}",
        4 * PTY_STOP_PHASE_BOUND
    );
    assert!(
        pty_stop_outer_bound() >= PTY_NESTED_RUNNER_STARTUP_BOUND + PTY_STOP_INNER_BOUND,
        "the outer runner can kill a conforming inner lifecycle: outer={:?}",
        pty_stop_outer_bound()
    );
}

/// The foreground job: the relay's stop path on a real controlling terminal, ending by the
/// relay's own redelivery of the `SIGTERM` the leader sends once raw mode is re-entered.
fn run_native_pty_stop_probe() {
    let witness = crate::native_tty::capture_process_stdio()
        .expect("the stop probe owns its foreground controlling terminal");
    // Ownership first, then raw mode: the leader treats raw mode as proof that SIGTSTP is
    // already owned, exactly as `run` acquires signals before relay setup. The fixture's
    // server thread predates ownership and so does not block SIGTSTP; it has finished its
    // attach by the time the session opens, and joining it here leaves the relay thread and
    // the keyboard worker (spawned after ownership, inheriting the blocked mask) as the only
    // threads a process-directed stop could reach.
    let (mut session, signals, _release, server_thread, _resize_tty) =
        relay_for_signal_exit(SignalExit::ReadFailure);
    server_thread
        .join()
        .expect("the fixture server finished its attach");
    assert!(
        signals.owns_stop(),
        "the foreground probe owns a default SIGTSTP"
    );
    let mut terminal = witness
        .enter_native_relay()
        .expect("the stop probe enters raw relay mode");
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(b"NATIVE_STOP_READY\n").unwrap();
    stdout.flush().unwrap();
    let pending_deadline = Instant::now() + PTY_STOP_PHASE_BOUND;
    while !owned_stop_pending().expect("querying the owned stop") {
        assert!(
            Instant::now() < pending_deadline,
            "no owned stop became pending"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    suspend_until_continued(&mut terminal, &mut session, &signals)
        .expect("the probe stops, continues, and re-enters raw mode");
    stdout.write_all(b"NATIVE_STOP_RESUMED\n").unwrap();
    stdout.flush().unwrap();
    drop(stdout);
    while FIRST_RELAY_SIGNAL.load(Ordering::SeqCst) == 0 {
        std::thread::sleep(Duration::from_millis(1));
    }
    let signal = FIRST_RELAY_SIGNAL.load(Ordering::SeqCst);
    finish_claimed_relay(
        &mut terminal,
        Ok(RelayStop::ExternalSignal(signal)),
        signals,
    )
    .expect("the restored prior signal action accepted redelivery");
}

/// A real kernel stop and continue through the relay, observed where they are observable: the
/// probe runs as the foreground job of a session leader on the same PTY, so its process group
/// is not orphaned and the kernel honours the default `SIGTSTP` the relay reveals.
#[test]
fn native_relay_stop_and_continue_are_observed_by_a_job_control_leader() {
    if std::env::var_os(PTY_STOP_LEADER).is_some() {
        exit_after_successful_fixture(run_native_pty_stop_leader);
    }
    if std::env::var_os(PTY_STOP_INNER).is_some() {
        exit_after_successful_fixture(run_native_pty_stop_inner);
    }
    run_isolated_signal_probe_bounded(
        PTY_STOP_INNER,
        "native_relay_stop_and_continue_are_observed_by_a_job_control_leader",
        pty_stop_outer_bound(),
    )
    .expect("the bounded stop lifecycle runner");
}

/// Drive one suspend/resume transition directly, standing in for the moment `pump` returns
/// `Suspend`. With no stop pending, `reveal_stop` is the spec's "SIGCONT cancelled the stop
/// before reveal" case: it unblocks and returns at once without parking this test process, so
/// the whole restore-then-reactivate sequence runs deterministically in one thread.
fn suspend_probe(
    scripted_flag_results: &[Option<rustix::io::Errno>],
) -> (
    RawRelayTerminal,
    rustix::termios::Termios,
    bool,
    std::ffi::c_int,
    Result<(), String>,
) {
    let mut raw = raw_relay_terminal_on_test_pty();
    let cooked = raw.baseline_termios();
    // Idle input keeps the keyboard worker alive so `park_keyboard` observes a live worker
    // instead of the operator-detach short circuit; the socket is otherwise irrelevant here.
    let (mut session, signals, _release, server_thread, _resize_tty) =
        relay_for_signal_exit(SignalExit::ReadFailure);
    assert!(
        signals.owns_stop(),
        "the isolated probe owns a default SIGTSTP"
    );
    RESIZED.store(false, Ordering::SeqCst);
    queue_status_flag_results(scripted_flag_results.iter().copied());

    let outcome = suspend_until_continued(&mut raw.terminal, &mut session, &signals);
    // Read the forced-resize latch before the guard drop clears the process signal state.
    let resized = RESIZED.load(Ordering::SeqCst);
    // Clear the recorded termination so the guard drop does not redeliver a real signal that
    // would kill this probe; the value is returned for the caller to assert on.
    let first = FIRST_RELAY_SIGNAL.swap(0, Ordering::SeqCst);
    RELAY_SIGNAL_EVENTS.store(0, Ordering::SeqCst);

    drop(session);
    drop(signals);
    server_thread.join().unwrap();
    (raw, cooked, resized, first, outcome)
}

#[test]
fn suspend_restores_the_terminal_then_reenters_raw_and_forces_a_resize() {
    if !run_isolated_signal_probe(
        SIGNAL_OWNER_PROBE,
        "suspend_restores_the_terminal_then_reenters_raw_and_forces_a_resize",
    ) {
        return;
    }
    use rustix::fs::{OFlags, fcntl_getfl};
    use rustix::termios::tcgetattr;

    let (raw, cooked, resized, _first, outcome) = suspend_probe(&[]);
    outcome.expect("the suspend transition completes without a real stop");

    let resumed = tcgetattr(raw.terminal.stdin()).unwrap();
    assert_ne!(
        resumed.local_modes & !rustix::termios::LocalModes::PENDIN,
        cooked.local_modes,
        "the relay did not re-enter raw mode after continue"
    );
    assert!(
        fcntl_getfl(raw.terminal.stdin())
            .unwrap()
            .contains(OFlags::NONBLOCK),
        "stdin was left blocking after resume"
    );
    assert!(resized, "resume did not force geometry reconciliation");
}

#[test]
fn suspend_writes_passive_cleanup_bytes_before_the_stop() {
    if !run_isolated_signal_probe(
        SIGNAL_OWNER_PROBE,
        "suspend_writes_passive_cleanup_bytes_before_the_stop",
    ) {
        return;
    }
    let expected = {
        let mut bytes = b"\x1b[?2004l".to_vec();
        bytes.extend_from_slice(&marion_tui::guard::leave_bytes());
        bytes
    };
    let (raw, _cooked, _resized, _first, outcome) = suspend_probe(&[]);
    outcome.expect("the suspend transition completes");

    // The passive cleanup bytes were written to the PTY while the terminal was restored for
    // the stop; they lead whatever the resumed relay later produced.
    let mut seen = Vec::new();
    let mut buf = [0u8; 512];
    let deadline = Instant::now() + Duration::from_secs(1);
    while seen.len() < expected.len() && Instant::now() < deadline {
        match raw.master.read(&mut buf) {
            Ok(0) => break,
            Ok(count) => seen.extend_from_slice(&buf[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1))
            }
            Err(error) => panic!("reading the suspend probe pty: {error}"),
        }
    }
    assert!(
        seen.starts_with(&expected),
        "passive cleanup bytes were not written before the stop: {:?}",
        String::from_utf8_lossy(&seen)
    );
}

#[test]
fn a_failed_raw_reentry_after_continue_rolls_back_to_the_cooked_baseline() {
    if !run_isolated_signal_probe(
        SIGNAL_OWNER_PROBE,
        "a_failed_raw_reentry_after_continue_rolls_back_to_the_cooked_baseline",
    ) {
        return;
    }
    use rustix::termios::tcgetattr;
    // restore_for_suspend changes the stdin and stdout flags (two calls); the raw re-entry's
    // own stdin flag change is the third, and it is the one made to fail.
    let (raw, cooked, _resized, _first, outcome) =
        suspend_probe(&[None, None, Some(rustix::io::Errno::BADF)]);

    let error = outcome.expect_err("a failed raw re-entry ends the transition with an error");
    assert!(
        error.contains("re-entering native terminal relay mode after continue"),
        "{error}"
    );
    let rolled_back = tcgetattr(raw.terminal.stdin()).unwrap();
    assert_eq!(
        rolled_back.local_modes & !rustix::termios::LocalModes::PENDIN,
        cooked.local_modes,
        "a failed raw re-entry must roll back to the cooked baseline"
    );
}

#[test]
fn a_termination_during_the_stop_wins_after_continue_before_raw_reentry() {
    if !run_isolated_signal_probe(
        SIGNAL_OWNER_PROBE,
        "a_termination_during_the_stop_wins_after_continue_before_raw_reentry",
    ) {
        return;
    }
    use rustix::termios::tcgetattr;
    // While the relay is stopped with termination handlers blocked, a SIGTERM arrives. It
    // stays pending until the resume unblocks the handlers, then dominates: the relay must
    // stay restored and finish through the termination path rather than re-enter raw mode.
    *WHILE_STOPPED_WITH_TERMINATIONS_BLOCKED
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(Box::new(|| on_relay_signal(SIGTERM)));

    let (raw, cooked, resized, first, outcome) = suspend_probe(&[]);
    outcome.expect("a late termination ends the transition cleanly, not as an error");

    assert_eq!(
        first, SIGTERM,
        "the termination that arrived during the stop was not recorded for the relay to finish"
    );
    let after = tcgetattr(raw.terminal.stdin()).unwrap();
    assert_eq!(
        after.local_modes & !rustix::termios::LocalModes::PENDIN,
        cooked.local_modes,
        "a late termination must leave the terminal restored, not re-entered into raw mode"
    );
    assert!(
        !resized,
        "a late termination must not force a resize or restart the keyboard worker"
    );
}

fn summary(
    id: &str,
    parent: Option<&str>,
    state: marion_core::node::NodeState,
) -> marion_core::proto::NodeSummary {
    marion_core::proto::NodeSummary {
        widened: vec![],
        budget: None,
        changed: None,
        review_of: None,
        review: None,
        agent_id: AgentId(id.into()),
        parent_id: parent.map(|p| AgentId(p.into())),
        name: None,
        agent_type: "synthetic".into(),
        harness: marion_core::harness::Harness::Codex,
        harness_version: None,
        depth: u8::from(parent.is_some()),
        state,
        reap_state: marion_core::node::ReapState::Live,
        timeout: marion_core::encoding::Duration::from_secs(900),
        pane: false,
        started_at: None,
        ended_at: None,
        tokens: None,
        attention: None,
        endpoint: None,
        race: None,
        cancel: None,
        workflow: None,
    }
}

/// The status line counts the native node's **direct children** by the same rule the tree
/// screen's status row uses for running, and puts a blocked child in its own column rather
/// than the running one. An unrelated root and the node itself are not children.
#[test]
fn the_status_line_counts_direct_children_by_state() {
    use marion_core::contract::ExitStatus;
    use marion_core::node::{BlockReason, NodeState};
    let id = AgentId("native".into());
    let nodes = vec![
        summary("native", None, NodeState::Running),
        summary("c1", Some("native"), NodeState::Running),
        summary(
            "c2",
            Some("native"),
            NodeState::Blocked(BlockReason::Permission),
        ),
        summary("c3", Some("native"), NodeState::Exited(ExitStatus::Ok)),
        summary("c4", Some("native"), NodeState::Exited(ExitStatus::Failed)),
        summary("c5", Some("native"), NodeState::Idle),
        summary("other-root", None, NodeState::Running),
        summary("grandchild", Some("c1"), NodeState::Running),
    ];
    assert_eq!(
        status_line(&id, &nodes),
        "marion: 5 children · running 2 · blocked 1 · done 1 · failed 1 · attention 2"
    );
    assert_eq!(
        status_line(&id, &[]),
        "marion: 0 children · running 0 · blocked 0 · done 0 · failed 0"
    );
}

/// **Attention is counted over the native node's whole subtree**, not just its children: a
/// grandchild that failed is exactly what the operator in the native session cannot see from
/// the children's columns. The node itself, an unrelated root, and a parent cycle that leads
/// back to the node are not counted, and nothing needing attention leaves the clause off.
#[test]
fn the_status_line_counts_attention_across_the_whole_subtree() {
    use marion_core::contract::ExitStatus;
    use marion_core::node::{BlockReason, NodeState};
    let id = AgentId("native".into());
    let mut orphan = summary("lost", Some("c1"), NodeState::Idle);
    orphan.reap_state = marion_core::node::ReapState::Orphaned;
    let nodes = vec![
        summary(
            "native",
            Some("loop"),
            NodeState::Blocked(BlockReason::Descendants),
        ),
        summary("loop", Some("native"), NodeState::Running),
        summary("c1", Some("native"), NodeState::Running),
        summary("gc", Some("c1"), NodeState::Exited(ExitStatus::Failed)),
        summary("ggc", Some("gc"), NodeState::Exited(ExitStatus::TimedOut)),
        orphan,
        summary("other", None, NodeState::Exited(ExitStatus::Failed)),
    ];
    assert_eq!(
        status_line(&id, &nodes),
        "marion: 2 children · running 2 · blocked 0 · done 0 · failed 0 · attention 3"
    );
    let calm = vec![
        summary("native", None, NodeState::Running),
        summary("c1", Some("native"), NodeState::Running),
        summary("gc", Some("c1"), NodeState::Exited(ExitStatus::Ok)),
    ];
    assert_eq!(
        status_line(&id, &calm),
        "marion: 1 children · running 1 · blocked 0 · done 0 · failed 0"
    );
}

/// The overlay is one row, restored around: save the cursor, stop the scroll region above the
/// last row, go to it, clear it, paint reverse video, restore. The clear gives the whole screen
/// back as the region. Neither touches the alternate screen, so each is the same on either.
#[test]
fn the_status_overlay_is_one_saved_and_restored_last_row() {
    assert_eq!(
        status_overlay_bytes(24, 80, "marion: tree pending"),
        b"\x1b7\x1b[1;23r\x1b[24;1H\x1b[2K\x1b[7mmarion: tree pending\x1b[0m\x1b8".to_vec()
    );
    // Truncated to the terminal's width, on a character boundary.
    assert_eq!(
        status_overlay_bytes(10, 9, "marion: 0 children · running 0"),
        b"\x1b7\x1b[1;9r\x1b[10;1H\x1b[2K\x1b[7mmarion: 0\x1b[0m\x1b8".to_vec()
    );
    assert_eq!(
        status_clear_bytes(24),
        b"\x1b7\x1b[r\x1b[24;1H\x1b[2K\x1b8".to_vec()
    );
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
    /// Armed from this very `read`, which the keyboard worker runs on its own thread, so the
    /// hook lands in that thread's slot and no other fixture's worker can take it.
    after_write: Option<Box<dyn FnOnce() + Send>>,
}

impl Read for OneRead {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if let Some(hook) = self.after_write.take() {
            AFTER_KEYBOARD_INPUT_WRITE.with(|slot| *slot.borrow_mut() = Some(hook));
        }
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

fn pane_token() -> marion_core::proto::PaneReadyTokenV1 {
    marion_core::proto::PaneReadyTokenV1::new([0x5a; 32])
}

fn attach_response() -> Frame {
    attach_response_for(marion_core::proto::result::PaneAttach {
        cols: 80,
        rows: 24,
        writable: true,
        held_by: None,
        ended: false,
        pane_ready: Some(marion_core::proto::result::PaneReadyDescriptorV1 {
            token: pane_token(),
            cut: 0,
        }),
    })
}

fn attach_response_for(pane: marion_core::proto::result::PaneAttach) -> Frame {
    use marion_core::encoding::Duration;
    use marion_core::harness::Harness;
    use marion_core::node::{NodeState, ReapState};
    use marion_core::proto::model::{AttachMode, NodeSummary, ReplayPoint};

    Frame::Response(marion_core::proto::Response::ok(
        RequestId::Number(1),
        &MethodResult::NodeAttach(marion_core::proto::result::NodeAttachResult {
            node: NodeSummary {
                widened: vec![],
                budget: None,
                changed: None,
                review_of: None,
                review: None,
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
                started_at: None,
                ended_at: None,
                tokens: None,
                attention: None,
                endpoint: None,
                race: None,
                cancel: None,
                workflow: None,
            },
            mode: AttachMode::ResubscribeFrom(ReplayPoint {
                records: 0,
                src_seq: None,
            }),
            pane: Some(pane),
        }),
    ))
}

fn read_only_attach_response(ended: bool, held_by: Option<u64>) -> Frame {
    attach_response_for(marion_core::proto::result::PaneAttach {
        cols: 80,
        rows: 24,
        writable: false,
        held_by,
        ended,
        pane_ready: Some(marion_core::proto::result::PaneReadyDescriptorV1 {
            token: pane_token(),
            cut: 0,
        }),
    })
}

/// A vendor that exits at once — `claude --version` is the shipped example — is gone before
/// the relay that claimed it can attach, so the supervisor answers read-only with `ended`.
/// That is not a lost writer lease: the relay must open, replay what the vendor said, and end.
/// Refusing here threw away the one thing the operator asked for.
#[test]
fn a_pane_that_ended_before_the_native_attach_is_replayed_rather_than_refused() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_thread = std::thread::spawn(move || {
        let mut lines = BufReader::new(server.try_clone().unwrap());
        assert!(matches!(read_frame(&mut lines), Frame::Request(_)));
        server
            .write_all(read_only_attach_response(true, None).to_line().as_bytes())
            .unwrap();
        server.flush().unwrap();
        // No geometry: a resize is a write, and this relay has no write half. The next frame
        // is the replay gate itself.
        assert!(matches!(
            read_frame(&mut lines),
            Frame::Input(note) if matches!(note.input, Input::NodePaneReady(_))
        ));
        server
            .write_all(
                pane_frame(
                    0,
                    PaneFrameKindV1::Output {
                        bytes: marion_core::proto::OpaquePaneBytesV1::new(b"VENDOR_SAID_THIS"),
                    },
                )
                .to_line()
                .as_bytes(),
            )
            .unwrap();
        server
            .write_all(pane_frame(1, PaneFrameKindV1::End {}).to_line().as_bytes())
            .unwrap();
        server.flush().unwrap();
    });

    let sink = Sink::default();
    let mut session = RawPaneSession::open_for_test(
        client,
        AgentId("native".into()),
        IdleInput,
        sink.clone(),
        (80, 24),
    )
    .expect("a pane that ended is read-only, not a refused claim");
    session.pump().expect("the relay reaches End");
    server_thread.join().unwrap();

    assert_eq!(
        sink.0.lock().unwrap().as_slice(),
        b"VENDOR_SAID_THIS",
        "the ended vendor's output never reached the operator's terminal"
    );
}

/// A read-only relay must not type. The supervisor answers an unleased opaque keystroke by
/// closing the connection (`Departure::PaneInputFailed`), so one stray key during the replay
/// of an ended pane would cost the operator the very output this session exists to show.
/// Detach still works - the operator's way out is not a write.
#[test]
fn a_read_only_native_relay_swallows_keystrokes_but_still_detaches() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_thread = std::thread::spawn(move || {
        let mut lines = BufReader::new(server.try_clone().unwrap());
        assert!(matches!(read_frame(&mut lines), Frame::Request(_)));
        server
            .write_all(read_only_attach_response(true, None).to_line().as_bytes())
            .unwrap();
        server.flush().unwrap();
        assert!(matches!(
            read_frame(&mut lines),
            Frame::Input(note) if matches!(note.input, Input::NodePaneReady(_))
        ));
        // The detach below is the barrier: the keyboard worker fed both bytes before it left,
        // so an empty read here is proof the forwardable one was never sent.
        let mut after = String::new();
        lines.read_line(&mut after).unwrap();
        assert_eq!(
            after, "",
            "a relay with no write half sent the supervisor input"
        );
    });

    let mut session = RawPaneSession::open_for_test(
        client,
        AgentId("native".into()),
        // One forwardable byte, then the operator's detach.
        std::io::Cursor::new(vec![0xff, 0x1d, b'd']),
        Sink::default(),
        (80, 24),
    )
    .expect("a pane that ended is read-only, not a refused claim");
    assert!(
        matches!(session.pump(), Ok(RelayStop::Complete)),
        "the operator could not leave a read-only relay"
    );
    drop(session);
    server_thread.join().unwrap();
}

/// Operator input fed from a channel: what arrives is returned as one read, silence is
/// `WouldBlock` so the keyboard worker keeps polling, and a dropped sender is EOF.
struct ChannelInput(mpsc::Receiver<Vec<u8>>);

impl Read for ChannelInput {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        match self.0.recv_timeout(Duration::from_millis(5)) {
            Ok(bytes) => {
                output[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Err(std::io::ErrorKind::WouldBlock.into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(0),
        }
    }
}

fn sink_text(sink: &Sink) -> String {
    String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned()
}

fn until_sink(sink: &Sink, what: &str, cond: impl Fn(&str) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond(&sink_text(sink)) {
        assert!(
            Instant::now() < deadline,
            "{what} never reached the operator's terminal; what it holds: {:?}",
            sink_text(sink)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `^] s` puts marion's one status row on the operator's terminal and `^] s` takes it off
/// again. The row is a `tree/subscribe` on the claimed connection, counted from the native
/// node's direct children and kept current by the same notifications the tree screen folds:
/// a subscribe answer with two children paints their counts, a `node/state` that fails one
/// repaints them, and the toggle-off is one clear of that row after which marion writes
/// nothing more to the terminal. The node's own bytes are never touched, and neither `^] s`
/// ever reaches the node.
#[test]
fn the_status_toggle_paints_the_last_row_from_the_tree_and_clears_it_again() {
    use marion_core::contract::ExitStatus;
    use marion_core::node::NodeState;

    let (client, mut server) = UnixStream::pair().unwrap();
    let (flip_tx, flip_rx) = mpsc::channel::<()>();
    let server_thread = std::thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut lines = complete_attach(&mut server);
        let subscribe = read_frame(&mut lines);
        let Frame::Request(request) = subscribe else {
            panic!("the toggle did not subscribe to the tree: {subscribe:?}");
        };
        assert_eq!(request.id, RequestId::Number(2));
        assert!(matches!(request.call, Call::TreeSubscribe(_)));
        let snapshot = marion_core::proto::result::TreeSubscribeResult {
            nodes: vec![
                summary("native", None, NodeState::Running),
                summary("c1", Some("native"), NodeState::Running),
                summary("c2", Some("native"), NodeState::Exited(ExitStatus::Ok)),
                summary("other-root", None, NodeState::Running),
            ],
            read_point: marion_core::proto::model::ReplayPoint {
                records: 0,
                src_seq: None,
            },
        };
        server
            .write_all(
                Frame::Response(marion_core::proto::Response::ok(
                    RequestId::Number(2),
                    &MethodResult::TreeSubscribe(snapshot),
                ))
                .to_line()
                .as_bytes(),
            )
            .unwrap();
        flip_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the test's cue to fail a child");
        server
            .write_all(
                Frame::Notification(marion_core::proto::Notification::new(Event::NodeState {
                    agent_id: AgentId("c1".into()),
                    state: NodeState::Exited(ExitStatus::Failed),
                    reap_state: marion_core::node::ReapState::Live,
                    ts: marion_core::encoding::SystemTime::from_unix_millis(0),
                }))
                .to_line()
                .as_bytes(),
            )
            .unwrap();
        // The detach is the barrier: after the subscribe, the node was told only that it lost
        // the row and got it back, and was sent no input.
        let after = frames_until_eof(&mut lines);
        assert_eq!(resizes(&after), vec![(80, 23), (80, 24)], "{after:?}");
        assert_eq!(
            pane_writes(&after),
            0,
            "a status toggle reached the node as input"
        );
    });

    let (keys_tx, keys_rx) = mpsc::channel::<Vec<u8>>();
    let sink = Sink::default();
    let mut session = RawPaneSession::open_for_test(
        client,
        AgentId("native".into()),
        ChannelInput(keys_rx),
        sink.clone(),
        (80, 24),
    )
    .unwrap();
    let (stop_tx, stop_rx) = mpsc::channel();
    let pump = std::thread::spawn(move || {
        let outcome = session.pump();
        drop(session);
        stop_tx.send(outcome).unwrap();
    });

    assert_eq!(
        sink_text(&sink),
        "",
        "marion painted before it was asked to"
    );
    keys_tx.send(vec![0x1d, b's']).unwrap();
    let first = String::from_utf8(status_overlay_bytes(
        24,
        80,
        "marion: 2 children · running 1 · blocked 0 · done 1 · failed 0",
    ))
    .unwrap();
    until_sink(&sink, "the first status row", |text| text.contains(&first));
    let text = sink_text(&sink);
    let pending = text
        .find("marion: tree pending")
        .expect("the row is painted before the subscribe is answered");
    assert!(pending < text.find(&first).unwrap());

    flip_tx.send(()).unwrap();
    let failed = String::from_utf8(status_overlay_bytes(
        24,
        80,
        "marion: 2 children · running 0 · blocked 0 · done 1 · failed 1 · attention 1",
    ))
    .unwrap();
    until_sink(&sink, "the repainted status row", |text| {
        text.contains(&failed)
    });

    keys_tx.send(vec![0x1d, b's']).unwrap();
    let clear = String::from_utf8(status_clear_bytes(24)).unwrap();
    until_sink(&sink, "the cleared status row", |text| {
        text.ends_with(&clear)
    });
    // Long enough for several throttled redraws, had the toggle-off not been honoured.
    std::thread::sleep(Duration::from_millis(600));
    let text = sink_text(&sink);
    assert!(
        text.ends_with(&clear),
        "marion kept painting after the status row was toggled off: {text:?}"
    );
    assert!(
        !text[..text.len() - clear.len()].ends_with(&clear),
        "the toggle-off cleared the row more than once"
    );

    keys_tx.send(vec![0x1d, b'd']).unwrap();
    let outcome = stop_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the relay did not end on the detach");
    assert!(matches!(outcome, Ok(RelayStop::Complete)), "{outcome:?}");
    pump.join().unwrap();
    server_thread.join().unwrap();
}

/// Every frame the fake supervisor reads after the attach, until the relay hangs up.
fn frames_until_eof(lines: &mut BufReader<UnixStream>) -> Vec<Frame> {
    let mut frames = Vec::new();
    loop {
        let mut line = String::new();
        if lines.read_line(&mut line).unwrap() == 0 {
            return frames;
        }
        frames.push(Frame::from_line(&line).unwrap());
    }
}

fn resizes(frames: &[Frame]) -> Vec<(u16, u16)> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Input(note) => match &note.input {
                Input::NodeResize { cols, rows, .. } => Some((*cols, *rows)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn pane_writes(frames: &[Frame]) -> usize {
    frames
        .iter()
        .filter(|frame| {
            matches!(frame, Frame::Input(note) if matches!(note.input, Input::NodePaneWrite(_)))
        })
        .count()
}

fn output_frame(seq: u64, bytes: &[u8]) -> Vec<u8> {
    pane_frame(
        seq,
        PaneFrameKindV1::Output {
            bytes: marion_core::proto::OpaquePaneBytesV1::new(bytes),
        },
    )
    .to_line()
    .into_bytes()
}

/// While the row is shown it is marion's, not the node's: the node is told its window is one
/// row shorter, the operator's scroll region stops above the row so the node's scrolling
/// cannot carry it away, and toggling off gives the row back — margins reset, the row cleared,
/// and the full height sent, which is the resize edge the harness repaints on.
#[test]
fn showing_the_status_row_takes_the_last_row_from_the_node_and_hiding_it_gives_it_back() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_thread = std::thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut lines = complete_attach(&mut server);
        frames_until_eof(&mut lines)
    });
    let (keys_tx, keys_rx) = mpsc::channel::<Vec<u8>>();
    let sink = Sink::default();
    let mut session = RawPaneSession::open_for_test(
        client,
        AgentId("native".into()),
        ChannelInput(keys_rx),
        sink.clone(),
        (80, 24),
    )
    .unwrap();
    let pump = std::thread::spawn(move || {
        let outcome = session.pump();
        drop(session);
        outcome
    });

    keys_tx.send(vec![0x1d, b's']).unwrap();
    until_sink(&sink, "the reserved row's scroll region", |text| {
        text.contains("\x1b[1;23r") && text.contains("marion: tree pending")
    });
    keys_tx.send(vec![0x1d, b's']).unwrap();
    until_sink(&sink, "the released scroll region", |text| {
        text.ends_with("\x1b7\x1b[r\x1b[24;1H\x1b[2K\x1b8")
    });
    keys_tx.send(vec![0x1d, b'd']).unwrap();
    assert!(matches!(pump.join().unwrap(), Ok(RelayStop::Complete)));
    let frames = server_thread.join().unwrap();

    assert_eq!(
        resizes(&frames),
        vec![(80, 23), (80, 24)],
        "the node was not told its window lost, then regained, the status row: {frames:?}"
    );
    assert_eq!(pane_writes(&frames), 0, "a status toggle reached the node");
}

/// The row is repainted only because something could have disturbed it: a change to its
/// text, or node output since the last paint (a clear screen, a screen switch). An idle node
/// with the row shown gets no bytes from marion at all.
#[test]
fn an_idle_node_gets_no_repaints_and_node_output_gets_one() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let (cue_tx, cue_rx) = mpsc::channel::<()>();
    let server_thread = std::thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut lines = complete_attach(&mut server);
        cue_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the test's cue to write");
        server
            .write_all(&output_frame(0, b"\x1b[2Jcleared"))
            .unwrap();
        frames_until_eof(&mut lines)
    });
    let (keys_tx, keys_rx) = mpsc::channel::<Vec<u8>>();
    let sink = Sink::default();
    let mut session = RawPaneSession::open_for_test(
        client,
        AgentId("native".into()),
        ChannelInput(keys_rx),
        sink.clone(),
        (80, 24),
    )
    .unwrap();
    let pump = std::thread::spawn(move || {
        let outcome = session.pump();
        drop(session);
        outcome
    });
    keys_tx.send(vec![0x1d, b's']).unwrap();
    until_sink(&sink, "the status row", |text| text.contains("marion:"));
    let painted = sink_text(&sink);
    std::thread::sleep(super::STATUS_REDRAW * 3);
    assert_eq!(
        sink_text(&sink),
        painted,
        "marion repainted the row of an idle node"
    );
    cue_tx.send(()).unwrap();
    until_sink(&sink, "the row repainted after the clear", |text| {
        text.rfind("cleared")
            .is_some_and(|at| text[at..].contains("marion:"))
    });
    keys_tx.send(vec![0x1d, b'd']).unwrap();
    assert!(matches!(pump.join().unwrap(), Ok(RelayStop::Complete)));
    server_thread.join().unwrap();
}

/// A relay that leaves while the row is shown — here a detach — gives the operator's shell the
/// whole screen and the node its whole height, since nothing marion paints stays behind.
#[test]
fn leaving_with_the_row_shown_gives_the_screen_and_the_row_back() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_thread = std::thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut lines = complete_attach(&mut server);
        frames_until_eof(&mut lines)
    });
    let (keys_tx, keys_rx) = mpsc::channel::<Vec<u8>>();
    let sink = Sink::default();
    let mut session = RawPaneSession::open_for_test(
        client,
        AgentId("native".into()),
        ChannelInput(keys_rx),
        sink.clone(),
        (80, 24),
    )
    .unwrap();
    let pump = std::thread::spawn(move || {
        let outcome = session.pump();
        (session, outcome)
    });
    keys_tx.send(vec![0x1d, b's']).unwrap();
    until_sink(&sink, "the status row", |text| text.contains("marion:"));
    keys_tx.send(vec![0x1d, b'd']).unwrap();
    let (mut session, outcome) = pump.join().unwrap();
    assert!(matches!(outcome, Ok(RelayStop::Complete)), "{outcome:?}");

    session.release_status().unwrap();
    drop(session);
    let frames = server_thread.join().unwrap();
    assert!(
        sink_text(&sink).ends_with(&String::from_utf8(status_clear_bytes(24)).unwrap()),
        "the scroll region and the row were left on the operator's terminal: {:?}",
        sink_text(&sink)
    );
    assert_eq!(resizes(&frames), vec![(80, 23), (80, 24)], "{frames:?}");
}

/// While the row is shown, a node that widens the scroll region past its own last row —
/// codex resets its region after inserting history, `CSI r` — gets the region stopped above
/// the row again right after that sequence, or its next scroll would carry the row into its
/// text. With the row hidden the same bytes pass through untouched.
#[test]
fn a_node_that_widens_its_scroll_region_gets_it_stopped_above_the_row() {
    const WIDENING: &[u8] = b"\x1b[r\x1b[5;24rA\x1b[2;9rB\x1bcC\x1b[!pD";
    let (client, mut server) = UnixStream::pair().unwrap();
    let (cue_tx, cue_rx) = mpsc::channel::<()>();
    let server_thread = std::thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut lines = complete_attach(&mut server);
        server.write_all(&output_frame(0, WIDENING)).unwrap();
        cue_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the test's cue that the row is shown");
        server.write_all(&output_frame(1, WIDENING)).unwrap();
        frames_until_eof(&mut lines)
    });
    let (keys_tx, keys_rx) = mpsc::channel::<Vec<u8>>();
    let sink = Sink::default();
    let mut session = RawPaneSession::open_for_test(
        client,
        AgentId("native".into()),
        ChannelInput(keys_rx),
        sink.clone(),
        (80, 24),
    )
    .unwrap();
    let pump = std::thread::spawn(move || {
        let outcome = session.pump();
        drop(session);
        outcome
    });

    let hidden = String::from_utf8(WIDENING.to_vec()).unwrap();
    until_sink(&sink, "the node's bytes", |text| text == hidden);
    keys_tx.send(vec![0x1d, b's']).unwrap();
    until_sink(&sink, "the status row", |text| text.contains("marion:"));
    let before = sink_text(&sink).len();
    cue_tx.send(()).unwrap();
    until_sink(&sink, "the node's second frame", |text| {
        text[before..].contains('D')
    });
    // What follows `D` is the row repainted: RIS cleared the screen it was on.
    let after = sink_text(&sink)[before..].to_string();
    assert!(
        after.starts_with(
            "\x1b[r\x1b[1;23r\x1b[5;24r\x1b[5;23rA\x1b[2;9rB\x1bc\x1b[1;23rC\x1b[!p\x1b7\x1b[1;23r\x1b8D"
        ),
        "the widened region was not stopped above the row: {after:?}"
    );
    keys_tx.send(vec![0x1d, b'd']).unwrap();
    assert!(matches!(pump.join().unwrap(), Ok(RelayStop::Complete)));
    server_thread.join().unwrap();
}

/// A pane frame is whatever one pty read returned, so it can end inside an escape sequence:
/// opencode's `CSI 30;6H` arrived as `CSI 30;` and `6H…`, and a row painted between them
/// ended the CSI early and printed `30;6H` into the composer. The row waits for the sequence
/// to finish, and is then painted after it, never inside it.
#[test]
fn the_status_row_is_never_painted_inside_a_sequence_a_pane_frame_split() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let (rest_tx, rest_rx) = mpsc::channel::<()>();
    let server_thread = std::thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut lines = complete_attach(&mut server);
        server.write_all(&output_frame(0, b"\x1b[30;")).unwrap();
        rest_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the test's cue to finish the sequence");
        server.write_all(&output_frame(1, b"6HPassed.")).unwrap();
        frames_until_eof(&mut lines)
    });
    let (keys_tx, keys_rx) = mpsc::channel::<Vec<u8>>();
    let sink = Sink::default();
    let mut session = RawPaneSession::open_for_test(
        client,
        AgentId("native".into()),
        ChannelInput(keys_rx),
        sink.clone(),
        (80, 24),
    )
    .unwrap();
    let pump = std::thread::spawn(move || {
        let outcome = session.pump();
        drop(session);
        outcome
    });

    until_sink(&sink, "the first half of the node's CSI", |text| {
        text == "\x1b[30;"
    });
    keys_tx.send(vec![0x1d, b's']).unwrap();
    // Several redraw periods: a row that did not wait would have been painted by now.
    std::thread::sleep(super::STATUS_REDRAW * 3);
    assert_eq!(
        sink_text(&sink),
        "\x1b[30;",
        "marion wrote into the middle of the node's escape sequence"
    );
    rest_tx.send(()).unwrap();
    until_sink(&sink, "the status row", |text| text.contains("marion:"));
    let text = sink_text(&sink);
    assert!(
        text.starts_with("\x1b[30;6HPassed.\x1b7"),
        "the row was not painted after the completed sequence: {text:?}"
    );
    keys_tx.send(vec![0x1d, b'd']).unwrap();
    assert!(matches!(pump.join().unwrap(), Ok(RelayStop::Complete)));
    server_thread.join().unwrap();
}

/// The refusal this relay still owes the operator: read-only because a *live* node's keyboard
/// belongs to somebody else. Relaying that would show a screen the operator cannot drive and
/// cannot explain, so the claim is reported as lost.
#[test]
fn a_writer_lease_held_by_another_connection_still_refuses_the_native_claim() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_thread = std::thread::spawn(move || {
        let mut lines = BufReader::new(server.try_clone().unwrap());
        assert!(matches!(read_frame(&mut lines), Frame::Request(_)));
        server
            .write_all(
                read_only_attach_response(false, Some(7))
                    .to_line()
                    .as_bytes(),
            )
            .unwrap();
        server.flush().unwrap();
    });

    let opened = RawPaneSession::open_for_test(
        client,
        AgentId("native".into()),
        IdleInput,
        Sink::default(),
        (80, 24),
    );
    server_thread.join().unwrap();
    let Err(error) = opened else {
        panic!("a live node whose keyboard is taken is not this relay's pane")
    };

    assert!(
        error.contains("did not retain its writer lease"),
        "the refusal lost its sentence: {error}"
    );
}

fn pane_frame(seq: u64, frame: PaneFrameKindV1) -> Frame {
    Frame::Notification(marion_core::proto::Notification::new(Event::NodePaneFrame(
        marion_core::proto::PaneFrameV1::new(AgentId("native".into()), seq, frame),
    )))
}

fn read_frame(lines: &mut BufReader<UnixStream>) -> Frame {
    let mut line = String::new();
    lines.read_line(&mut line).unwrap();
    Frame::from_line(&line).unwrap()
}

/// The fake supervisor's half of an attach, read through the reader it returns — which the
/// caller must keep reading from. A second `BufReader` over the same socket would lose whatever
/// this one read ahead: on a loaded machine the relay's next frame (a status `tree/subscribe`,
/// a resize) can arrive in the same `read` as the pane-ready line, and a fresh reader would
/// then wait for a frame already consumed.
fn complete_attach(server: &mut UnixStream) -> BufReader<UnixStream> {
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
    lines
}

#[derive(Clone, Copy, Debug)]
enum SignalExit {
    End,
    Detach,
    ReadFailure,
    WriteFailure,
    ResizeFailure,
}

/// The last element is the resize terminal, **master and slave together**. Both, because on
/// Linux the last close of a pty master hangs the slave up: `TIOCGWINSZ` on it then answers
/// `EIO` and `isatty` says no, and the relay's geometry read — which is the first thing
/// `open_with_io` does — fails before the session exists. Darwin keeps answering on an
/// orphaned slave, which is why holding only the slave passed here for as long as it did.
type SignalRelay = (
    RawPaneSession<Box<dyn Write>>,
    RelaySignalGuard,
    Option<mpsc::SyncSender<()>>,
    std::thread::JoinHandle<()>,
    Option<(crate::pty::PtyMaster, std::os::fd::OwnedFd)>,
);

fn relay_for_signal_exit(exit: SignalExit) -> SignalRelay {
    let (client, mut server) = UnixStream::pair().unwrap();
    let (attached_tx, attached_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let server_thread = std::thread::spawn(move || {
        complete_attach(&mut server);
        // `ResizeFailure` and `ReadFailure` ask the same socket to be closed and differ only
        // in *which* of the relay's two uses of it meets the closed end first — `pump`
        // forwards a pending resize before it reads. Letting this thread simply fall off the
        // end would close the socket at a moment nothing orders against the relay: on a fast
        // machine the drop wins and the resize write gets `EPIPE`, on a loaded runner the
        // write lands in a still-open socket and the *read* reports `closed before End`
        // instead. Closing **before** the rendezvous — a close, not a `shutdown`, because a
        // half-closed `AF_UNIX` peer still accepts a write on macOS — makes the failure this
        // fixture is named for the only one reachable.
        if matches!(exit, SignalExit::ResizeFailure) {
            drop(server);
            attached_tx.send(()).unwrap();
            return;
        }
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
            SignalExit::ReadFailure => {}
            SignalExit::ResizeFailure => unreachable!("closed before the rendezvous above"),
            SignalExit::WriteFailure => {
                server
                    .write_all(
                        pane_frame(
                            0,
                            PaneFrameKindV1::Output {
                                bytes: marion_core::proto::OpaquePaneBytesV1::new(b"output"),
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
    // The master is named rather than left a temporary: dropping it here would close the last
    // master descriptor and hang the slave up before the geometry read below. See
    // [`SignalRelay`].
    let resize_master = crate::pty::PtyMaster::open(crate::pty::WinSize::new(80, 24)).unwrap();
    let resize_tty = resize_master.open_slave().unwrap();
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
        Some((resize_master, resize_tty)),
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
fn pump_reports_a_pending_owned_stop_as_suspend_without_consuming_it() {
    if !run_isolated_signal_probe(
        SIGNAL_RESET_PROBE,
        "pump_reports_a_pending_owned_stop_as_suspend_without_consuming_it",
    ) {
        return;
    }
    let _restore_original = install_sentinel();
    let (mut session, signals, release, server_thread, _resize_tty) =
        relay_for_signal_exit(SignalExit::Detach);
    assert!(signals.owns_stop());
    // SAFETY: SIGTSTP is blocked on this thread, so the raise only marks it pending.
    assert_eq!(unsafe { raise(SIGTSTP) }, 0);

    let outcome = session.pump();

    assert_eq!(outcome, Ok(RelayStop::Suspend));
    assert!(
        owned_stop_pending().expect("querying pending signals"),
        "the pump observed the stop but must not consume it"
    );
    // Discard the pending stop before ownership is released so this probe is not stopped.
    let _ignored = install_ignored_action(SIGTSTP);
    release.unwrap().send(()).unwrap();
    drop(session);
    drop(signals);
    server_thread.join().unwrap();
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
    // Shared, not moved: the hook runs *inside* `open_with_io`, just before the geometry read,
    // so a master owned by the hook would be dropped while the read still needs it. On Linux
    // that closes the last master descriptor and the slave answers `EIO`. The test keeps the
    // other handle for the whole session.
    let resize_tty =
        std::sync::Arc::new(crate::pty::PtyMaster::open(crate::pty::WinSize::new(80, 24)).unwrap());
    let input = resize_tty.open_slave().unwrap();
    let input_fd = input.as_raw_fd();
    let (client, mut server) = UnixStream::pair().unwrap();
    let hook_tty = std::sync::Arc::clone(&resize_tty);
    *AFTER_RESIZE_SIGNAL_ACQUIRE
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(Box::new(move || {
        hook_tty
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
            Frame::Input(note) if matches!(note.input, marion_core::proto::Input::NodeResize { cols: 111, rows: 37, .. })
        ));
        assert!(matches!(
            read_frame(&mut lines),
            Frame::Input(note) if matches!(note.input, marion_core::proto::Input::NodePaneReady(_))
        ));

        let tail = [0xff, 0x00, 0x9b, b'3', b'1', b'm', b'Z'];
        server
            .write_all(
                pane_frame(
                    0,
                    PaneFrameKindV1::Output {
                        bytes: marion_core::proto::OpaquePaneBytesV1::new(tail),
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
            after_write: None,
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
                let MethodResult::NodeAttach(mut attached) = marion_core::proto::Method::NodeAttach
                    .decode_result(match &response.outcome {
                        marion_core::proto::Outcome::Result(body) => body,
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
                *response = marion_core::proto::Response::ok(
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

/// The supervisor refuses opaque pane input by **closing the negotiated connection**
/// (`serve::Departure::PaneInputFailed`: a notification has no response envelope). Seen from
/// the relay that is an EOF right after a keystroke went out, and the operator must be told
/// *that* — after the terminal is back in cooked mode, as the primary error, not a generic
/// "closed before End" that reads like a network fault.
///
/// The refusal is a race the relay must not lose: the EOF reaches the pump as soon as the
/// supervisor closes, and the worker is still on the far side of its own write. So the worker
/// is held here between the bytes going out and its next instruction, and the pump reaches its
/// verdict inside that window — the flag has to have been advanced before the write, the same
/// order `apply_frame` keeps, or a keystroke the supervisor demonstrably received reads back
/// as a bare disconnection.
///
/// Mutation: report every pre-End EOF with one message, let a cleanup error displace the
/// primary one, or advance the flag after the write instead of before it.
#[test]
fn a_supervisor_input_refusal_ends_the_relay_visibly_after_terminal_restoration() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let (pump_reached_its_verdict, worker_waits) = mpsc::channel::<()>();
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
            panic!("the keystroke did not arrive as an Input notification")
        };
        assert!(matches!(note.input, Input::NodePaneWrite(_)));
        // What `Outbound::fail(Departure::PaneInputFailed { .. })` does to the wire.
        server.shutdown(std::net::Shutdown::Both).unwrap();
        release.store(true, Ordering::SeqCst);
    });

    let mut session = RawPaneSession::open_for_test(
        client,
        AgentId("native".into()),
        OneRead {
            bytes: Some(b"k".to_vec()),
            hold,
            after_write: Some(Box::new(move || {
                let _ = worker_waits.recv_timeout(PANE_VERDICT_BOUND);
            })),
        },
        Sink::default(),
        (80, 24),
    )
    .unwrap();
    let refusal = session.pump().unwrap_err();
    let _ = pump_reached_its_verdict.send(());
    server_thread.join().unwrap();
    assert!(
        refusal.contains("refused") && refusal.contains("keyboard input"),
        "the operator is not told the input was refused: {refusal}"
    );
    assert!(
        !refusal.contains("closed before End"),
        "a refusal must not read as a network fault: {refusal}"
    );

    // Through the finish stage: every cleanup succeeded, so the refusal is the whole message,
    // and it is what `relay_native_facade` prints once the terminal is restored.
    let finished = resolve_relay_finish(Err(refusal.clone()), Ok(()), Ok(()), Ok(None), |_| {
        panic!("no signal was captured")
    });
    assert_eq!(finished, Err(refusal));
}

/// A keyboard worker that fails must end the pump **now**: it shuts the socket's read side so
/// the pump's `poll` returns at once, and the pump reports the worker's failure rather than the
/// EOF it caused.
///
/// Causal, not timed: the server stays silent until the pump has returned, so nothing but the
/// worker's wake can end it. A generous backstop lets a broken wake fail rather than hang: the
/// server then hangs up by itself, and the test sees that it had to.
///
/// Mutation: drop the shutdown (the pump waits for the backstop's hang-up and the backstop
/// assertion fails), or let the EOF message displace the worker's (the message assertion fails).
#[test]
fn a_keyboard_failure_wakes_the_pump_immediately_with_its_own_message() {
    struct FailingInput;
    impl Read for FailingInput {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("sentinel terminal input failure"))
        }
    }

    const BACKSTOP: Duration = Duration::from_secs(10);
    let (client, mut server) = UnixStream::pair().unwrap();
    let (pumped, pump_returned) = std::sync::mpsc::channel::<()>();
    // The server completes the attach and then says nothing until the pump has returned.
    let server_thread = std::thread::spawn(move || {
        complete_attach(&mut server);
        let gave_up = pump_returned.recv_timeout(BACKSTOP).is_err();
        drop(server);
        gave_up
    });
    let mut session = RawPaneSession::open_for_test(
        client,
        AgentId("native".into()),
        FailingInput,
        Sink::default(),
        (80, 24),
    )
    .unwrap();
    let error = session.pump().unwrap_err();
    let _ = pumped.send(());
    let gave_up = server_thread.join().unwrap();
    assert!(
        !gave_up,
        "the pump ended only when the silent server hung up, not on the worker's stop: {error}"
    );
    assert!(
        error.contains("sentinel terminal input failure"),
        "the pump reported something other than the keyboard failure: {error}"
    );
    drop(session);
}

/// The terminal output descriptor is nonblocking while the relay runs, and a TUI frame is a
/// burst larger than a pty's output queue. A writer that surfaced `WouldBlock` would end the
/// relay mid-frame whenever the operator's terminal drained a little slower than the harness
/// painted, which is the E2E failure `native_facade_e2e.rs` first caught: the client left with
/// the frame's tail unwritten. So a full queue is waited out, not reported.
#[test]
fn output_writes_wait_out_a_full_nonblocking_terminal_queue() {
    let (writer_end, mut reader_end) = UnixStream::pair().unwrap();
    writer_end.set_nonblocking(true).unwrap();
    let payload = vec![0x41u8; 1 << 20];
    let expected = payload.len();
    let reader = std::thread::spawn(move || {
        let mut received = 0usize;
        let mut chunk = [0u8; 4096];
        loop {
            // A terminal emulator that repaints between reads: slower than the burst.
            std::thread::sleep(Duration::from_millis(2));
            match reader_end.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => received += count,
                Err(error) => panic!("reading the drained output: {error}"),
            }
        }
        received
    });
    let mut writer = super::OwnedFdWriter(std::os::fd::OwnedFd::from(writer_end));
    writer
        .write_all(&payload)
        .expect("a momentarily full terminal queue is waited out, not reported");
    drop(writer);
    assert_eq!(
        reader.join().unwrap(),
        expected,
        "every byte of the burst arrived"
    );
}

/// The same descriptor is nonblocking when the finish stage writes its passive cleanup bytes,
/// and at a detach the queue is whatever the harness's last frame left in it — a full one when
/// the operator detaches under a large dialog (`native_facade_e2e.rs`'s copilot lane, 1.0.83's
/// folder-trust dialog: `passive terminal bytes` failed with `EAGAIN` and the clean detach
/// exited 1). A full queue at cleanup is waited out exactly as it is mid-relay.
#[test]
fn passive_cleanup_waits_out_a_full_nonblocking_terminal_queue() {
    let (writer_end, mut reader_end) = UnixStream::pair().unwrap();
    writer_end.set_nonblocking(true).unwrap();
    // Fill the queue the way a frame larger than the terminal drains would leave it.
    let mut filled = 0usize;
    loop {
        match rustix::io::write(&writer_end, &[0x41u8; 4096]) {
            Ok(written) => filled += written,
            Err(rustix::io::Errno::AGAIN) => break,
            Err(error) => panic!("filling the terminal queue: {error}"),
        }
    }
    let mut expected = vec![0x41u8; filled];
    write_passive_terminal_cleanup_to(&mut expected).unwrap();
    let reader = std::thread::spawn(move || {
        // The terminal drains only once the cleanup write is already refused.
        std::thread::sleep(Duration::from_millis(100));
        let mut received = Vec::new();
        reader_end.read_to_end(&mut received).unwrap();
        received
    });
    use std::os::fd::AsFd;
    write_passive_terminal_cleanup_to(BorrowedTerminalWriter(writer_end.as_fd()))
        .expect("a full terminal queue at cleanup is waited out, not reported");
    drop(writer_end);
    assert_eq!(
        reader.join().unwrap(),
        expected,
        "the cleanup bytes followed the frame that filled the queue"
    );
}
