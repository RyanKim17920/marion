//! The `Harness` enum (design §3.1 key table, §3.2 `Node.harness`).
//!
//! `harness` is a **required enum key** in §3.1's agent-type format, not free text. Carrying it as
//! a `String` meant nothing could dispatch on it: a typo resolved to a type that then compiled the
//! wrong harness's argv with no error anywhere. This is that key as a type.
//!
//! The wire spelling is the enum's only external form — an agent-type file writes it, and
//! `TaskContract.child.harness` records it — so `FromStr`/`Display`/serde are all one table and
//! round-trip exactly. Pure data: nothing here reads a file.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The harnesses marion knows how to name. Naming one is not the same as having an adapter for it
/// — `marion_harness::adapter_for` is where "known" narrows to "implemented".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Harness {
    ClaudeCode,
    Codex,
    Gemini,
    OpenCode,
}

impl Harness {
    /// The wire spelling. The one place the strings live.
    pub const fn as_str(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude-code",
            Harness::Codex => "codex",
            Harness::Gemini => "gemini",
            Harness::OpenCode => "opencode",
        }
    }

    /// Every harness marion can name — what `marion doctor` would list.
    pub const ALL: [Harness; 4] = [
        Harness::ClaudeCode,
        Harness::Codex,
        Harness::Gemini,
        Harness::OpenCode,
    ];
}

/// An unrecognised `harness:` value. §3.1: unknown keys are a **load error**, never a silent
/// default — defaulting would compile some other harness's argv for a type that asked for one
/// marion has never heard of.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown harness {0:?}; known harnesses are claude-code, codex, gemini, opencode")]
pub struct UnknownHarness(pub String);

impl FromStr for Harness {
    type Err = UnknownHarness;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Harness::ALL
            .into_iter()
            .find(|h| h.as_str() == s)
            .ok_or_else(|| UnknownHarness(s.to_string()))
    }
}

impl fmt::Display for Harness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Serialized as the bare wire string, matching what `TaskContract.child.harness` has always
/// written. A `#[derive(Serialize)]` would have produced `"ClaudeCode"` and changed the contract's
/// JSON, so the impls are hand-written against the same table as `FromStr`.
impl Serialize for Harness {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Harness {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_harness_round_trips_through_its_wire_spelling() {
        for h in Harness::ALL {
            assert_eq!(h.to_string().parse::<Harness>(), Ok(h));
        }
    }

    #[test]
    fn the_wire_spellings_are_the_ones_already_in_use() {
        // These strings are in persisted contracts and in agent-type frontmatter. Changing one is
        // a format change, not a rename.
        assert_eq!(Harness::ClaudeCode.as_str(), "claude-code");
        assert_eq!(Harness::Codex.as_str(), "codex");
        assert_eq!(Harness::Gemini.as_str(), "gemini");
        assert_eq!(Harness::OpenCode.as_str(), "opencode");
    }

    #[test]
    fn an_unknown_harness_is_a_typed_error_not_a_default() {
        let e = "claude_code".parse::<Harness>().unwrap_err();
        assert_eq!(e, UnknownHarness("claude_code".into()));
        assert!(
            e.to_string().contains("claude-code"),
            "the error lists the known spellings"
        );
    }

    #[test]
    fn serde_uses_the_wire_string_so_no_contract_json_changes() {
        assert_eq!(
            serde_json::to_string(&Harness::ClaudeCode).unwrap(),
            "\"claude-code\""
        );
        assert_eq!(
            serde_json::from_str::<Harness>("\"codex\"").unwrap(),
            Harness::Codex
        );
        // A derived enum would have written "ClaudeCode" here; that would rewrite every contract.
        assert!(serde_json::from_str::<Harness>("\"ClaudeCode\"").is_err());
    }
}
