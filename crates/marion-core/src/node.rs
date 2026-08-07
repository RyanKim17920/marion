//! `NodeState`, `BlockReason` and `ReapState` — design §3.2's registry model, as types.
//!
//! These were comments on the `Node` sketch until the journal needed to *write* them: a state
//! transition is the thing the journal records, so the states have to be a type before the record
//! can be one. `ExitStatus` is deliberately **not** re-declared here — it already exists in
//! [`crate::contract`], where a contract's `ResultStatus` aliases it, and §3.2 and §6.7 name one
//! set of outcomes, not two.

use serde::{Deserialize, Serialize};

use crate::contract::ExitStatus;

/// **A process's start identity: the one thing that distinguishes a survivor from a recycled pid.**
///
/// §7.2's probe branch — *"resolved by checking for the process"* — needs to know whether the
/// process this journal is about is the one wearing that pid *today*. A bare pid cannot answer it:
/// after a supervisor crash of unknown duration, nothing distinguishes a surviving node from an
/// unrelated process the kernel handed the same number. `restart.rs` refuses the probe for exactly
/// that reason, and is right to while this is absent.
///
/// **Opaque, platform-tagged, and compared only for equality.** The value inside is whatever the
/// platform's kernel reports as a process start time, in that platform's own units — microseconds
/// in a `timeval` on macOS, clock ticks since boot on Linux, one-second granularity from `lstart`.
/// Different units, different epochs, and on some platforms a granularity coarse enough that two
/// processes can share a value. So it is never parsed, never ordered, and never compared across
/// platforms: the tag is part of the string precisely so a journal carried between machines cannot
/// produce a false match. Equality is the only sound operation, and equality is all §7.2 needs.
///
/// Marion never derives a *time* from this. A reader wanting when a node started has
/// `Spawned`'s own `ts`, which is marion's observation in marion's units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartId(pub String);

impl std::fmt::Display for StartId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a node is held. §3.2: `Descendants` is §7.6's hold, resolved by a re-prompt or a timeout;
/// `Permission` and `Elicitation` await an answer and are resolved by one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockReason {
    Descendants,
    Permission,
    Elicitation,
}

/// §3.2's `state`. Externally tagged, so a payload-carrying variant reads as
/// `{"Exited":"TimedOut"}` and a unit one as `"Running"`. Pinned by test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeState {
    Spawning,
    Ready,
    Running,
    Idle,
    Blocked(BlockReason),
    Exited(ExitStatus),
}

impl NodeState {
    /// §7.6's gating set names `state == Exited(_)` as one of its two disjuncts; this is that
    /// half. The other half is [`ReapState::is_terminal_for_gating`], and **neither is the whole
    /// rule** — the rule is their disjunction, which lives with the gate, not here.
    pub fn is_exited(self) -> bool {
        matches!(self, NodeState::Exited(_))
    }

    pub fn exit_status(self) -> Option<ExitStatus> {
        match self {
            NodeState::Exited(s) => Some(s),
            _ => None,
        }
    }
}

/// §7.2. `Orphaned` is **not** an exit: it is what marion writes about a node whose fate it has no
/// record of deciding, and it is reached only by the restart policy — never by an observation.
/// Journal replay therefore never *produces* it (see [`crate::registry`]); the marking is a
/// decision taken over a replayed tree.
///
/// **`Orphaned` is not a claim that the process died, and no consumer may read it as one.** §7.2 is
/// explicit that it covers two physically different worlds — *"the process may be gone **or still
/// running with marion no longer attached**"* — and marion takes no liveness probe before writing
/// it, so it cannot distinguish them. It says what marion **knows**, which is nothing. A consumer
/// that needs "the process is gone" wants `NodeState::Exited(_)`, which is written from an
/// observation; a consumer that needs "marion will see nothing more from this node" wants
/// [`ReapState::is_terminal_for_gating`], which is what this state actually supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReapState {
    Live,
    ReapedIdle,
    Orphaned,
}

impl ReapState {
    /// §7.6, stated totally there: *"a descendant counts as terminal for descendant-gating iff
    /// `state == Exited(_)` OR `reap_state ∈ {Orphaned, ReapedIdle}`."*
    ///
    /// The property both non-`Live` states share is **not** that the process is gone — an earlier
    /// reading of this comment said so, and §7.2 contradicts it in as many words for `Orphaned`,
    /// which explicitly covers a node *"still running with marion no longer attached"*. What they
    /// share is that **marion will observe no further transition of this node**: `ReapedIdle`
    /// because marion ended the process itself and holds the transcript, `Orphaned` because marion
    /// is not attached to whatever may still be running. Neither can resolve itself *to marion*,
    /// and a gate waiting on one waits forever — which is what §7.6's rule is about, and why the
    /// rule survives the correction unchanged.
    ///
    /// The distinction matters because the two premises license different things. "The process is
    /// gone" would license reporting a death, reusing the node's resources, or telling an operator
    /// there is nothing running; none of those follow here, and §7.2's marking takes no liveness
    /// probe that could make them follow.
    pub fn is_terminal_for_gating(self) -> bool {
        matches!(self, ReapState::ReapedIdle | ReapState::Orphaned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_state_pins_its_wire_shape() {
        let cases = [
            (NodeState::Spawning, r#""Spawning""#),
            (NodeState::Ready, r#""Ready""#),
            (NodeState::Running, r#""Running""#),
            (NodeState::Idle, r#""Idle""#),
            (
                NodeState::Blocked(BlockReason::Descendants),
                r#"{"Blocked":"Descendants"}"#,
            ),
            (
                NodeState::Exited(ExitStatus::TimedOut),
                r#"{"Exited":"TimedOut"}"#,
            ),
        ];
        for (state, wire) in cases {
            assert_eq!(serde_json::to_string(&state).unwrap(), wire);
            assert_eq!(serde_json::from_str::<NodeState>(wire).unwrap(), state);
        }
    }

    #[test]
    fn reap_state_pins_its_wire_shape() {
        for (s, wire) in [
            (ReapState::Live, r#""Live""#),
            (ReapState::ReapedIdle, r#""ReapedIdle""#),
            (ReapState::Orphaned, r#""Orphaned""#),
        ] {
            assert_eq!(serde_json::to_string(&s).unwrap(), wire);
            assert_eq!(serde_json::from_str::<ReapState>(wire).unwrap(), s);
        }
    }

    #[test]
    fn the_gating_disjuncts_are_each_only_half_the_rule() {
        // §7.6 is emphatic that ReapedIdle and Orphaned gate *even though neither is an exit*.
        assert!(!NodeState::Running.is_exited());
        assert!(NodeState::Exited(ExitStatus::Ok).is_exited());
        assert!(!ReapState::Live.is_terminal_for_gating());
        assert!(ReapState::ReapedIdle.is_terminal_for_gating());
        assert!(ReapState::Orphaned.is_terminal_for_gating());
    }
}
