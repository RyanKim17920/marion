//! **What a node is called**, in words: its label, its short id, its state word, and which agent an
//! operator means by an id they typed. The tree row, Home Watch, `marion list`, the MCP tools'
//! replies, notifications and `marion export` all name a node through these, so no two of them can
//! call one node two things.
//!
//! Split out of [`crate::tree`] (which re-exports every item here) because the supervisor's own
//! replies name nodes too, and the tree's drawing half is client-only.

use marion_core::contract::AgentId;
use marion_core::node::{NodeState, ReapState};
use marion_core::proto::NodeSummary;

/// **What one node is called, in the one place that decides it.**
///
/// The tree row and the detail pane's title are the same string by construction rather than by
/// agreement: two spellings of a node's name is how an operator ends up unsure whether the `8ea3`
/// in the sidebar and the `01a091ba-8ea3-…` in the pane are the same agent.
/// An endpoint node's `provider:model` follows, so which model a node runs on is on its row.
pub fn label_of(node: &NodeSummary) -> String {
    let name = node
        .name
        .clone()
        .unwrap_or_else(|| format!("{} {}", node.agent_type, short_id(&node.agent_id.0)));
    let label = match &node.endpoint {
        Some(e) => format!("{name} {}", e.label()),
        None => name,
    };
    let label = match review_note(node).or_else(|| race_note(node)) {
        Some(note) => format!("{label} · {note}"),
        None => label,
    };
    let label = match workflow_note(node) {
        Some(note) => format!("{label} · {note}"),
        None => label,
    };
    match widened_note(node) {
        Some(note) => format!("{label} · {note}"),
        None => label,
    }
}

/// **What a seat row adds**: its seat, and once the race is decided, how it came out. `None` for a
/// node in no race. The tree and Home Watch both say it through this.
pub fn race_note(node: &NodeSummary) -> Option<String> {
    use marion_core::race::SeatVerdict;
    let badge = node.race.as_ref()?;
    let seat = format!("agent {}", badge.seat);
    Some(match badge.verdict {
        None => seat,
        Some(v) => {
            let word = match v {
                SeatVerdict::Won => "★ won",
                SeatVerdict::Lost => "lost",
                SeatVerdict::Failed => "failed",
                SeatVerdict::Cancelled => "cancelled",
                SeatVerdict::Unlaunched => "not started",
            };
            format!("{seat} {word}")
        }
    })
}

/// **What a workflow step's row adds**: its step, and the review round past the first. `None` for a
/// node in no workflow run. The tree and Home Watch both say it through this.
pub fn workflow_note(node: &NodeSummary) -> Option<String> {
    let badge = node.workflow.as_ref()?;
    let step = if badge.step_id.is_empty() {
        format!("step {}", u16::from(badge.step) + 1)
    } else {
        format!("step {}", badge.step_id)
    };
    Some(match badge.round {
        0 => step,
        r => format!("{step} r{}", u16::from(r) + 1),
    })
}

/// **What a node's row says when the operator's opt-in let it start with more than its caller**:
/// "uncontained child (operator opt-in)" when its caller ran sandboxed and it does not, else the
/// axes it widened. `None` for every node within its caller's authority.
pub fn widened_note(node: &NodeSummary) -> Option<String> {
    match node.widened.as_slice() {
        [] => None,
        [one] if one == "containment" => Some("uncontained child (operator opt-in)".into()),
        axes => Some(format!(
            "wider than its parent: {} (operator opt-in)",
            axes.join(", ")
        )),
    }
}

/// **What a reviewer row adds**: that it is a review, and once its report is read, how many
/// findings it made and how many block. `None` for every node that is not a reviewer. The tree
/// and Home Watch both say it through this, so the two cannot count differently.
pub fn review_note(node: &NodeSummary) -> Option<String> {
    node.review_of.as_ref()?;
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    Some(match node.review {
        Some(t) if t.blocking > 0 => format!(
            "review: {}, {} blocking",
            plural(t.findings, "finding"),
            t.blocking
        ),
        Some(t) => format!("review: {}", plural(t.findings, "finding")),
        None if node.state.is_exited() => "review: no report read".to_string(),
        None => "reviewing".to_string(),
    })
}

/// The part of an id that tells one node from its siblings.
///
/// An `AgentId` is a UUIDv7: the first group is a timestamp every node spawned in the same second
/// shares, so the second group is the earliest one that varies. Anything not shaped like a UUID —
/// a test fixture's `root-claude` — is shown whole, because guessing at its structure would hide
/// characters the caller chose.
pub fn short_id(id: &str) -> &str {
    let uuid_shaped = id.len() == 36
        && id.is_ascii()
        && id
            .char_indices()
            .all(|(i, c)| (c == '-') == matches!(i, 8 | 13 | 18 | 23));
    if uuid_shaped { &id[9..13] } else { id }
}

/// **The agent an operator means by `arg`**, out of the ids they could have meant — the one rule
/// every command that takes an agent uses (`ls`, `attach`, `steer`, `cancel`, `resume`, and the
/// MCP tools' `id`).
///
/// A whole id is itself. Otherwise the one id whose [`short_id`] is `arg` — what the tree row
/// shows, so an operator can type what they see — or which starts with `arg`, so a copied prefix
/// works too. More than one candidate is refused with every whole id named, because guessing
/// between them would act on an agent nobody chose. An arg that names nothing is returned as-is:
/// the supervisor's own `not found` sentence is the better answer, and a second one here would be a
/// second spelling of it.
pub fn resolve_node<'a>(
    arg: &str,
    ids: impl IntoIterator<Item = &'a str>,
) -> Result<AgentId, String> {
    let ids: Vec<&str> = ids.into_iter().collect();
    if arg.is_empty() || ids.contains(&arg) {
        return Ok(AgentId(arg.to_string()));
    }
    let matches: Vec<&str> = ids
        .into_iter()
        .filter(|id| short_id(id) == arg || id.starts_with(arg))
        .collect();
    match matches.as_slice() {
        [] => Ok(AgentId(arg.to_string())),
        [one] => Ok(AgentId((*one).to_string())),
        many => Err(format!(
            "`{arg}` matches {} agents ({}); name the one you mean by more of its id",
            many.len(),
            many.join(", ")
        )),
    }
}

/// A node's state in one short word.
///
/// `reap_state` is folded in rather than shown beside it, and only when it is not `Live`: §7.2 is
/// emphatic that `Orphaned` is *not* a claim the process died, so it is shown as its own word
/// instead of being collapsed into an exit. A node that is both `Exited` and reaped shows the exit,
/// which is the more specific fact.
pub fn state_label(state: NodeState, reap: ReapState) -> String {
    match (state, reap) {
        (NodeState::Exited(s), _) => format!("exited:{s:?}").to_lowercase(),
        (_, ReapState::Orphaned) => "orphaned".into(),
        (_, ReapState::ReapedIdle) => "reaped".into(),
        (NodeState::Blocked(r), _) => format!("blocked:{r:?}").to_lowercase(),
        (s, ReapState::Live) => format!("{s:?}").to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A unique prefix of a whole id names its agent too**, so an id copied short works; a prefix
    /// several ids share is refused with each of them, like an ambiguous short id.
    #[test]
    fn a_unique_prefix_of_a_whole_id_names_its_agent() {
        let a = "01a091ba-8ea3-7000-8000-000000000001";
        let b = "01a091bb-5b04-7000-8000-000000000003";
        assert_eq!(resolve_node("01a091ba", [a, b]), Ok(AgentId(a.into())));
        assert_eq!(resolve_node("01a091bb-5", [a, b]), Ok(AgentId(b.into())));
        let both = resolve_node("01a091", [a, b]).expect_err("both start with it");
        assert!(both.contains(a) && both.contains(b), "{both}");
        assert_eq!(resolve_node("", [a]), Ok(AgentId(String::new())));
    }

    #[test]
    fn a_state_is_labelled_by_the_more_specific_of_the_two_fields() {
        use marion_core::contract::ExitStatus;
        assert_eq!(state_label(NodeState::Idle, ReapState::Live), "idle");
        assert_eq!(
            state_label(NodeState::Running, ReapState::Orphaned),
            "orphaned",
            "§7.2: orphaned is not an exit, so it is its own word"
        );
        assert_eq!(
            state_label(NodeState::Exited(ExitStatus::Ok), ReapState::Orphaned),
            "exited:ok",
            "an exit is the more specific fact"
        );
        assert!(
            state_label(
                NodeState::Blocked(marion_core::node::BlockReason::Permission),
                ReapState::Live
            )
            .contains("permission")
        );
    }
}
