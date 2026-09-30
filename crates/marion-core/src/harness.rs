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
    OpenCode,
    /// GitHub Copilot CLI, headless `-p` (measured on 1.0.83, `tests/fixtures/s24/`). The sixth
    /// name and the fifth binary; added after `Acp`'s doc comment below was written, so where that
    /// comment says "the other four" read "the other five".
    Copilot,
    /// Block's goose, headless `run -t` (measured on 1.49.0, `tests/fixtures/s26/`). The seventh
    /// name and the sixth binary.
    Goose,
    /// Cline CLI, headless positional prompt with `--json` (measured on 3.0.61,
    /// `tests/fixtures/s27/`). The eighth name and the seventh binary.
    Cline,
    /// Qwen Code, headless `-p` (measured on 0.23.0, `tests/fixtures/s25/`). The ninth name and
    /// the eighth binary; a Gemini CLI fork whose headless surface is Claude Code's shape.
    Qwen,
    /// Google's Antigravity CLI, headless `-p` with `--output-format stream-json` (measured on
    /// 1.2.8, `tests/fixtures/s32/`). The tenth name and the ninth binary; not a Gemini CLI fork,
    /// and it speaks no ACP. Wire name `agy`, the binary's own.
    Antigravity,
    /// pi (earendil-works, formerly `badlogic/pi-mono`), headless `-p --mode json` (measured on
    /// 0.80.2, `tests/fixtures/s34-pi/`). The eleventh name and the tenth binary, and the first with
    /// no MCP client of its own: marion's declaration is an extension it loads per run.
    Pi,
    /// **A protocol, not a vendor** — §5.2's `acp` adapter row, *"one adapter serving many
    /// agents"*. The other four name a binary; this one names Agent Client Protocol and the binary
    /// arrives per launch (`Extras::acp_agent`), because the agent supplies its own identity in
    /// `initialize` rather than being looked up.
    ///
    /// That asymmetry is §3.3's, not an accident of spelling: for the other four marion knows the
    /// version before it launches anything, and for this one it does not.
    Acp,
}

impl Harness {
    /// The wire spelling. The one place the strings live.
    pub const fn as_str(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude-code",
            Harness::Codex => "codex",
            Harness::OpenCode => "opencode",
            Harness::Copilot => "copilot",
            Harness::Goose => "goose",
            Harness::Cline => "cline",
            Harness::Qwen => "qwen",
            Harness::Antigravity => "agy",
            Harness::Pi => "pi",
            Harness::Acp => "acp",
        }
    }

    /// The name a person types and reads: [`Self::as_str`], except `claude` for Claude Code, the
    /// spelling `marion claude` and the agent types use. Output meant for an operator (doctor's
    /// rows) says this; the wire, the journal and contracts keep [`Self::as_str`].
    pub const fn cli_name(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude",
            other => other.as_str(),
        }
    }

    /// Every harness marion can name — what `marion doctor` would list.
    pub const ALL: [Harness; 10] = [
        Harness::ClaudeCode,
        Harness::Codex,
        Harness::OpenCode,
        Harness::Copilot,
        Harness::Goose,
        Harness::Cline,
        Harness::Qwen,
        Harness::Antigravity,
        Harness::Pi,
        Harness::Acp,
    ];
}

/// An unrecognised `harness:` value. §3.1: unknown keys are a **load error**, never a silent
/// default — defaulting would compile some other harness's argv for a type that asked for one
/// marion has never heard of.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "unknown harness {0:?}; known harnesses are claude, codex, opencode, copilot, goose, cline, \
     qwen, agy, pi, acp (claude-code is accepted for claude)"
)]
pub struct UnknownHarness(pub String);

impl FromStr for Harness {
    type Err = UnknownHarness;

    /// The wire spelling, plus `claude` for Claude Code: the name a person types everywhere else
    /// (agent types, `marion claude`), accepted as input while the wire keeps `claude-code`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "claude" {
            return Ok(Harness::ClaudeCode);
        }
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
        assert_eq!(Harness::OpenCode.as_str(), "opencode");
        assert_eq!(Harness::Copilot.as_str(), "copilot");
        assert_eq!(Harness::Goose.as_str(), "goose");
        assert_eq!(Harness::Cline.as_str(), "cline");
        assert_eq!(Harness::Qwen.as_str(), "qwen");
        // The binary's own name: Google's Antigravity CLI installs as `agy`.
        assert_eq!(Harness::Antigravity.as_str(), "agy");
        assert_eq!(Harness::Pi.as_str(), "pi");
        // §5.2's adapter row is spelled `acp`, and it names the protocol rather than an agent.
        assert_eq!(Harness::Acp.as_str(), "acp");
    }

    /// **`claude` is the spelling a person types**: agent types, `marion claude` and the README
    /// all say it, so `.marion/agents.toml`'s `harness =` and `doctor --harness` accept it. The
    /// wire spelling stays `claude-code`, which persisted contracts carry, and still parses.
    #[test]
    fn claude_parses_as_claude_code_and_the_wire_spelling_is_unchanged() {
        assert_eq!("claude".parse::<Harness>(), Ok(Harness::ClaudeCode));
        assert_eq!("claude-code".parse::<Harness>(), Ok(Harness::ClaudeCode));
        for h in Harness::ALL {
            assert_eq!(h.cli_name().parse::<Harness>(), Ok(h), "{h}");
        }
        assert_eq!(Harness::ClaudeCode.cli_name(), "claude");
        assert_eq!(Harness::ClaudeCode.as_str(), "claude-code");
        let e = "clod".parse::<Harness>().unwrap_err().to_string();
        assert!(e.contains("known harnesses are claude,"), "{e}");
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
