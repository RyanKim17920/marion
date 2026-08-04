//! `NodeState`, `BlockReason` and `ReapState` — design §3.2's registry model, as types.
//!
//! These were comments on the `Node` sketch until the journal needed to *write* them: a state
//! transition is the thing the journal records, so the states have to be a type before the record
//! can be one. `ExitStatus` is deliberately **not** re-declared here — it already exists in
//! [`crate::contract`], where a contract's `ResultStatus` aliases it, and §3.2 and §6.7 name one
//! set of outcomes, not two.

use serde::{Deserialize, Serialize};

use crate::contract::ExitStatus;

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
/// decision taken over a replayed tree, and it is not this milestone's work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReapState {
    Live,
    ReapedIdle,
    Orphaned,
}

impl ReapState {
    /// §7.6, stated totally there: *"a descendant counts as terminal for descendant-gating iff
    /// `state == Exited(_)` OR `reap_state ∈ {Orphaned, ReapedIdle}`."* Both non-`Live` states
    /// share the property that forces it — the process is gone and the node cannot resolve itself.
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
