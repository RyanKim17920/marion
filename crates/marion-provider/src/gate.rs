//! **A hold on the provider's answers**, so a test can decide *when* a node's next turn exists.
//!
//! # Why this is here and not a sleep in the test
//!
//! §6.1 step 8's rule — a launcher MUST NOT substitute a sleep for an observation — binds a test
//! exactly as it binds a launcher, and it binds hardest on a test whose whole subject is an
//! ordering. §7.3.3's re-attach has one assertion that a replay-only implementation cannot pass:
//! *a client receives events emitted **after** it attached*. Writing that assertion means causing
//! an event **after** an attach, and "after" has to be a fact rather than an interval — a sleep
//! there does not remove the race, it encodes it, and the test then passes on a fast machine for
//! the same reason the bug ships.
//!
//! So the provider stops. A held turn is a node that is genuinely parked with nothing further to
//! say, for as long as the test wants, and the release is the cause of everything that follows.
//!
//! # The hold is placed after the log and before the answer, deliberately
//!
//! `server::serve_connection` already appends to [`crate::reqlog`] *before* answering, so that the
//! evidence survives a panicking script. Holding at that same point means the request is **on
//! disk while it is still held** — which is what makes the ordering checkable by a third party
//! rather than asserted by the test about itself. The test reads the provider's own log, sees that
//! the turns it is about to cause have **not** been asked for, attaches, and only then releases.
//! *"The provider had not been asked at attach time"* is then a fact in a file, not a comment.
//!
//! # Counting is per wire, and per wire is what makes it deterministic
//!
//! Arrival order across the port is a race by construction (`lib.rs`: Claude Code issues a
//! session-title request concurrently with its first real turn, measured 8 ms apart), which is why
//! nothing in `script.rs` counts. This does count — it has to, since "the second turn" is the thing
//! being held — so it counts within **one wire**, where the requests are one node's sequential
//! conversation and an auxiliary request on another wire cannot shift the numbering.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

/// What the server consults between logging a request and answering it.
///
/// Two implementations, and they hold on different evidence: [`TurnGate`] counts one wire's turns,
/// [`Rendezvous`] recognises *which node* is asking. The trait exists because
/// [`crate::CannedServer`] must be able to take either without the two growing a common enum, and
/// because a third kind of hold is a test's business rather than this crate's.
pub trait Hold: std::fmt::Debug + Send + Sync {
    /// `wire` is [`crate::wire_name`]'s spelling, or `None` for a request nothing could classify;
    /// `body` is the parsed request. A hold that returns is a request the server may now answer.
    fn wait_for(&self, wire: Option<&str>, body: &Value);
}

/// A hold on one wire's turns. Shared with the server; a test keeps a clone to release it.
#[derive(Debug)]
pub struct TurnGate {
    wire: String,
    hold_from: u64,
    seen: AtomicU64,
    parked: AtomicU64,
    released: Mutex<bool>,
    wake: Condvar,
}

impl TurnGate {
    /// Answer the first `hold_from - 1` requests on `wire`; hold that one and every later one
    /// until [`Self::release`].
    ///
    /// `wire` is [`crate::wire_name`]'s spelling — `"anthropic"`, `"responses"`, `"gemini"`,
    /// `"openai"`. A request on any other wire, and any request the provider could not classify,
    /// passes through untouched: a gate that held what it could not name would turn a script bug
    /// into a hang.
    pub fn holding_from(wire: &str, hold_from: u64) -> Arc<TurnGate> {
        assert!(hold_from >= 1, "requests are numbered from one");
        Arc::new(TurnGate {
            wire: wire.to_string(),
            hold_from,
            seen: AtomicU64::new(0),
            parked: AtomicU64::new(0),
            released: Mutex::new(false),
            wake: Condvar::new(),
        })
    }

    /// How many turns are held **right now**.
    ///
    /// The observation a test waits on, and the reason it never has to sleep: a non-zero count is
    /// the node itself reporting that it has asked its question and cannot proceed. It is not the
    /// thing asserted on — that is the request log, which a third party wrote.
    pub fn parked(&self) -> u64 {
        self.parked.load(Ordering::SeqCst)
    }

    /// Let every held turn, and every later one, through.
    pub fn release(&self) {
        let mut r = self.released.lock().unwrap_or_else(|e| e.into_inner());
        *r = true;
        self.wake.notify_all();
    }

    /// Called by the server between logging a request and answering it.
    pub(crate) fn wait_for(&self, wire: Option<&str>) {
        if wire != Some(self.wire.as_str()) {
            return;
        }
        if self.seen.fetch_add(1, Ordering::SeqCst) + 1 < self.hold_from {
            return;
        }
        let mut released = self.released.lock().unwrap_or_else(|e| e.into_inner());
        if *released {
            return;
        }
        self.parked.fetch_add(1, Ordering::SeqCst);
        while !*released {
            released = self.wake.wait(released).unwrap_or_else(|e| e.into_inner());
        }
        self.parked.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Hold for TurnGate {
    /// The body is not evidence this gate takes: its whole subject is *"the n-th turn on a wire"*,
    /// which is a fact about the sequence and not about the request.
    fn wait_for(&self, wire: Option<&str>, _body: &Value) {
        TurnGate::wait_for(self, wire)
    }
}

/// **A rendezvous between two nodes speaking the same wire**: hold each one's requests until every
/// named node has asked for a turn.
///
/// # Why counting cannot do this job
///
/// [`TurnGate`] counts requests *within a wire*, and its own doc says why that is deterministic:
/// there, the requests are one node's sequential conversation. Fan-in breaks that premise — two
/// children of one root, on one harness, speak one wire, and which of them reaches the provider
/// first is a race. "Hold the second anthropic request" would name child A's second turn on one
/// run and child B's first turn on the next.
///
/// So this holds on **identity** instead: each node's own task text carries a marker (the same
/// discriminator [`crate::RootScript::marker`] uses, and for the same reason — every harness
/// replays its node's task on every turn), and the hold lifts for everyone the moment the last
/// named marker has arrived. What that buys a test is an *ordering fact*: when child A is answered,
/// child B's process existed and had already asked the provider a question. Neither node can have
/// finished before the other started.
///
/// # It is bounded, and expiry is a verdict the test must read
///
/// A rendezvous whose other party never comes would park for ever, and a test that can only fail by
/// wedging the suite is not a test. So the wait is bounded, and expiry lets everything through
/// **and records that it did** — [`Rendezvous::expired`]. A test asserting concurrency must assert
/// `!expired()` and say what expiry means, because expiry is precisely the shape a serializing
/// implementation produces: the second node never started, so the first waited alone.
///
/// The bound is not a threshold on anything the test measures. Nothing is asserted about how long
/// the rendezvous took; the only readings are *met* or *expired*.
#[derive(Debug)]
pub struct Rendezvous {
    wire: String,
    markers: Vec<String>,
    bound: Duration,
    state: Mutex<Meeting>,
    wake: Condvar,
}

#[derive(Debug, Default)]
struct Meeting {
    arrived: BTreeSet<String>,
    expired: bool,
}

impl Rendezvous {
    /// Hold every request on `wire` that carries one of `markers`, until all of them have.
    ///
    /// A request on another wire, or one carrying no marker, passes through untouched — the rule
    /// [`TurnGate::holding_from`] states, for the same reason: a hold on something it cannot name
    /// turns a script bug into a hang.
    pub fn on_wire<I, S>(wire: &str, markers: I, bound: Duration) -> Arc<Rendezvous>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let markers: Vec<String> = markers.into_iter().map(Into::into).collect();
        assert!(
            markers.len() >= 2,
            "a rendezvous of fewer than two parties is not a rendezvous"
        );
        Arc::new(Rendezvous {
            wire: wire.to_string(),
            markers,
            bound,
            state: Mutex::new(Meeting::default()),
            wake: Condvar::new(),
        })
    }

    /// The markers seen so far, in sorted order.
    pub fn arrived(&self) -> Vec<String> {
        self.lock().arrived.iter().cloned().collect()
    }

    /// Did every named party arrive?
    pub fn met(&self) -> bool {
        self.lock().arrived.len() == self.markers.len()
    }

    /// Did some party give up waiting because the bound ran out?
    ///
    /// **The reading a concurrency test asserts on.** `true` means at least one node sat at the
    /// rendezvous for the whole bound while another never appeared.
    pub fn expired(&self) -> bool {
        self.lock().expired
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Meeting> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Hold for Rendezvous {
    fn wait_for(&self, wire: Option<&str>, body: &Value) {
        if wire != Some(self.wire.as_str()) {
            return;
        }
        let Some(marker) = self
            .markers
            .iter()
            .find(|m| crate::script::carries(body, m))
        else {
            return;
        };
        let deadline = Instant::now() + self.bound;
        let mut state = self.lock();
        state.arrived.insert(marker.clone());
        // Woken for everyone, including the arrival that completes the set: the last party must not
        // block, and the earlier ones are waiting on exactly this.
        self.wake.notify_all();
        while state.arrived.len() < self.markers.len() && !state.expired {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                state.expired = true;
                self.wake.notify_all();
                break;
            }
            let (guard, _) = self
                .wake
                .wait_timeout(state, left)
                .unwrap_or_else(|e| e.into_inner());
            state = guard;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_testsupport::until;
    use serde_json::json;

    #[test]
    fn the_turns_before_the_hold_pass_and_the_hold_stops_the_rest_until_released() {
        let g = TurnGate::holding_from("responses", 2);
        // The first turn is answered with nothing waiting: if this blocked, the node would never
        // reach the turn the test is about.
        g.wait_for(Some("responses"));
        assert_eq!(g.parked(), 0);

        let held = {
            let g = Arc::clone(&g);
            std::thread::spawn(move || g.wait_for(Some("responses")))
        };
        assert!(until(|| g.parked() == 1), "the second turn is held");
        assert!(!held.is_finished(), "and stays held with no release");
        g.release();
        held.join().unwrap();
        assert_eq!(g.parked(), 0);
    }

    #[test]
    fn a_request_on_another_wire_is_neither_counted_nor_held() {
        let g = TurnGate::holding_from("responses", 1);
        // Would block for ever if it were counted, since `hold_from` is 1.
        g.wait_for(Some("anthropic"));
        g.wait_for(None);
        assert_eq!(g.parked(), 0);
    }

    #[test]
    fn a_turn_arriving_after_the_release_is_not_held() {
        let g = TurnGate::holding_from("responses", 1);
        g.release();
        g.wait_for(Some("responses"));
        assert_eq!(g.parked(), 0);
    }

    fn turn(text: &str) -> Value {
        json!({"messages": [{"role": "user", "content": [{"type": "text", "text": text}]}]})
    }

    /// Generous, and never measured: every assertion below is on *met* or *expired*, never on how
    /// long anything took.
    const AMPLE: Duration = Duration::from_secs(30);

    #[test]
    fn neither_party_is_answered_until_both_have_asked() {
        let rz = Rendezvous::on_wire("anthropic", ["MARK-A", "MARK-B"], AMPLE);
        let first = {
            let rz = Arc::clone(&rz);
            std::thread::spawn(move || rz.wait_for(Some("anthropic"), &turn("do MARK-A now")))
        };
        assert!(
            until(|| rz.arrived() == vec!["MARK-A".to_string()]),
            "the first party's arrival is visible while it is still held"
        );
        assert!(!first.is_finished(), "and it is genuinely held");
        assert!(!rz.met());

        Hold::wait_for(&*rz, Some("anthropic"), &turn("do MARK-B now"));
        first.join().unwrap();
        assert!(rz.met());
        assert!(!rz.expired(), "both arrived, so nothing timed out");
    }

    /// The negative control for the test above: without it, `neither_party_is_answered…` would pass
    /// against a `Rendezvous` that held nothing at all and returned immediately.
    #[test]
    fn a_party_that_waits_alone_expires_rather_than_parking_for_ever() {
        let rz = Rendezvous::on_wire("anthropic", ["MARK-A", "MARK-B"], Duration::from_millis(50));
        Hold::wait_for(&*rz, Some("anthropic"), &turn("do MARK-A now"));
        assert!(rz.expired(), "the bound ran out and the hold says so");
        assert!(!rz.met());
    }

    #[test]
    fn another_wire_and_an_unmarked_body_are_neither_recorded_nor_held() {
        // Would block for the whole bound if either were taken as an arrival.
        let rz = Rendezvous::on_wire("anthropic", ["MARK-A", "MARK-B"], AMPLE);
        Hold::wait_for(&*rz, Some("responses"), &turn("do MARK-A now"));
        Hold::wait_for(&*rz, None, &turn("do MARK-A now"));
        Hold::wait_for(&*rz, Some("anthropic"), &turn("nothing to see"));
        assert!(rz.arrived().is_empty());
        assert!(!rz.expired());
    }
}
