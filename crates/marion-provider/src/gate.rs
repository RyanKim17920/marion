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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn until(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        cond()
    }

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
}
