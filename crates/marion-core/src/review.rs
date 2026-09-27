//! Cross-harness review: the policy a reviewer node runs under, and the pure rules marion uses to
//! turn a reviewer's text into a verdict.
//!
//! **marion decides the verdict, not the model.** A reviewer reports findings; marion blocks only
//! on a finding that is *grounded* (it names a file the change actually touched) and at least as
//! severe as the spec's threshold. A reviewer that says "block" while naming only files outside
//! the change does not block. Text that cannot be read at all is an error, never a block: an
//! unreadable review is an absent review, and calling it a rejection would let a malformed reply
//! veto work nobody judged.
//!
//! Pure data and pure functions, like the rest of this crate: running the reviewer node, reading
//! the diff and re-prompting the author belong to the supervisor.

use serde::{Deserialize, Serialize};

/// How severe a finding is. Ordered so that `Critical > High > Medium > Low`.
///
/// The variants are declared lowest first so the derived `Ord` is the severity order; the wire
/// spelling is the lowercase name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// The reviewer's spelling, case-insensitively. Only the four names are accepted: a reviewer
    /// that writes "severe" or "blocker" has not used the schema, and guessing a level for it could
    /// turn a remark into a block or a block into a remark.
    pub fn from_reviewer(s: &str) -> Option<Severity> {
        match s.trim().to_ascii_lowercase().as_str() {
            "critical" => Some(Severity::Critical),
            "high" => Some(Severity::High),
            "medium" => Some(Severity::Medium),
            "low" => Some(Severity::Low),
            _ => None,
        }
    }
}

/// The most rounds a review may run: the first review plus at most two re-reviews after the
/// author was re-prompted. A ceiling rather than a default so a typo cannot buy an unbounded loop.
pub const MAX_ROUNDS_CEILING: u8 = 3;

/// The review policy an agent type carries.
///
/// Read from configuration through [`RawReviewSpec`], so every bound is checked once, at the edge:
/// `max_rounds` must be `1..=3` and `timeout_secs` non-zero. Out-of-range values are refused, not
/// clamped, because a clamp would run a policy the operator did not write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawReviewSpec", into = "RawReviewSpec")]
pub struct ReviewSpec {
    /// The agent type that reviews; `None` lets the supervisor pick one from a different model
    /// family than the author's (see `model_family`).
    pub agent_type: Option<String>,
    /// The least severe grounded finding that blocks.
    pub block_on: Severity,
    /// Rounds including the first; `1..=MAX_ROUNDS_CEILING`.
    pub max_rounds: u8,
    /// Wall-clock bound on one review round.
    pub timeout_secs: u64,
    /// Extra instruction for the reviewer ("security", "the migration"), appended to its prompt.
    pub focus: Option<String>,
}

impl Default for ReviewSpec {
    fn default() -> Self {
        ReviewSpec {
            agent_type: None,
            block_on: Severity::High,
            max_rounds: 1,
            timeout_secs: 600,
            focus: None,
        }
    }
}

/// [`ReviewSpec`] as written: every key optional, unknown keys refused so a misspelt one is an
/// error rather than a silently ignored policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawReviewSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_on: Option<Severity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReviewSpecError {
    #[error("review max_rounds must be between 1 and {MAX_ROUNDS_CEILING}, got {0}")]
    MaxRounds(u8),
    #[error("review timeout_secs must be greater than 0")]
    ZeroTimeout,
}

impl TryFrom<RawReviewSpec> for ReviewSpec {
    type Error = ReviewSpecError;

    fn try_from(raw: RawReviewSpec) -> Result<Self, Self::Error> {
        let d = ReviewSpec::default();
        let max_rounds = raw.max_rounds.unwrap_or(d.max_rounds);
        if !(1..=MAX_ROUNDS_CEILING).contains(&max_rounds) {
            return Err(ReviewSpecError::MaxRounds(max_rounds));
        }
        let timeout_secs = raw.timeout_secs.unwrap_or(d.timeout_secs);
        if timeout_secs == 0 {
            return Err(ReviewSpecError::ZeroTimeout);
        }
        Ok(ReviewSpec {
            agent_type: raw.agent_type,
            block_on: raw.block_on.unwrap_or(d.block_on),
            max_rounds,
            timeout_secs,
            focus: raw.focus,
        })
    }
}

impl From<ReviewSpec> for RawReviewSpec {
    fn from(s: ReviewSpec) -> Self {
        RawReviewSpec {
            agent_type: s.agent_type,
            block_on: Some(s.block_on),
            max_rounds: Some(s.max_rounds),
            timeout_secs: Some(s.timeout_secs),
            focus: s.focus,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(toml_text: &str) -> Result<ReviewSpec, String> {
        toml::from_str::<ReviewSpec>(toml_text).map_err(|e| e.to_string())
    }

    #[test]
    fn severities_order_critical_above_high_above_medium_above_low() {
        assert!(Severity::Critical > Severity::High);
        assert!(Severity::High > Severity::Medium);
        assert!(Severity::Medium > Severity::Low);
    }

    #[test]
    fn a_severity_spells_lowercase_on_the_wire() {
        assert_eq!(
            serde_json::to_string(&Severity::Critical).unwrap(),
            r#""critical""#
        );
        assert_eq!(
            serde_json::from_str::<Severity>(r#""medium""#).unwrap(),
            Severity::Medium
        );
    }

    #[test]
    fn a_reviewers_severity_is_read_case_insensitively_and_only_by_its_four_names() {
        assert_eq!(Severity::from_reviewer(" HIGH "), Some(Severity::High));
        assert_eq!(
            Severity::from_reviewer("Critical"),
            Some(Severity::Critical)
        );
        assert_eq!(Severity::from_reviewer("low"), Some(Severity::Low));
        for guess in ["severe", "blocker", "info", "", "hi"] {
            assert_eq!(Severity::from_reviewer(guess), None, "{guess:?}");
        }
    }

    #[test]
    fn an_empty_review_table_is_the_documented_default() {
        let s = spec("").unwrap();
        assert_eq!(s, ReviewSpec::default());
        assert_eq!(s.block_on, Severity::High);
        assert_eq!(s.max_rounds, 1);
        assert_eq!(s.timeout_secs, 600);
        assert_eq!(s.agent_type, None);
        assert_eq!(s.focus, None);
    }

    #[test]
    fn every_key_is_read_when_written() {
        let s = spec(
            r#"
            agent_type = "codex-reviewer"
            block_on = "medium"
            max_rounds = 3
            timeout_secs = 90
            focus = "the migration"
            "#,
        )
        .unwrap();
        assert_eq!(
            s,
            ReviewSpec {
                agent_type: Some("codex-reviewer".into()),
                block_on: Severity::Medium,
                max_rounds: 3,
                timeout_secs: 90,
                focus: Some("the migration".into()),
            }
        );
    }

    #[test]
    fn max_rounds_outside_one_to_the_ceiling_is_refused_not_clamped() {
        assert!(spec("max_rounds = 1").is_ok());
        assert!(spec("max_rounds = 3").is_ok());
        let four = spec("max_rounds = 4").unwrap_err();
        assert!(four.contains("between 1 and 3, got 4"), "{four}");
        let zero = spec("max_rounds = 0").unwrap_err();
        assert!(zero.contains("got 0"), "{zero}");
    }

    #[test]
    fn a_zero_timeout_is_refused() {
        let e = spec("timeout_secs = 0").unwrap_err();
        assert!(e.contains("timeout_secs must be greater than 0"), "{e}");
    }

    #[test]
    fn a_misspelt_key_or_unknown_severity_is_refused() {
        assert!(spec("block_one = \"high\"").is_err());
        assert!(spec("block_on = \"severe\"").is_err());
    }

    #[test]
    fn a_spec_round_trips_through_json() {
        let s = ReviewSpec {
            agent_type: Some("r".into()),
            block_on: Severity::Critical,
            max_rounds: 2,
            timeout_secs: 30,
            focus: None,
        };
        let back: ReviewSpec = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
