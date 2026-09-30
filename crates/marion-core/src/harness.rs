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

/// **Wire spellings of harnesses marion once launched and no longer has a row for.**
///
/// Not a harness: nothing parses to one, and an agent type naming one is refused like any other
/// unknown name. It exists for the journal, which is append-only and outlives a retirement — a
/// record written while the harness was supported still names it, and replay must keep that
/// record rather than stop at it ([`crate::journal::decode`]).
///
/// `gemini`: Google's gemini CLI, retired upstream in favour of Antigravity (`agy`).
pub const RETIRED: &[&str] = &["gemini"];

/// `s` as its [`RETIRED`] entry, or `None` for a name this build never retired.
pub fn retired(s: &str) -> Option<&'static str> {
    RETIRED.iter().copied().find(|r| *r == s)
}

/// **A harness as a persisted file names it**: one this build has, or one it has retired.
///
/// What a contract (`TaskContract.child.harness`) and a node's `events.jsonl`
/// (`Payload::Vendor.harness`) carry. Both were written while the harness was supported, and both
/// outlive its retirement exactly as the journal does, so reading them must keep the record
/// rather than fail on it — the rule [`crate::journal::decode`] follows for the journal. A new
/// record always names a [`Harness`]: [`Self::Retired`] is only ever read, never launched.
///
/// The rescue is exactly [`RETIRED`]: a name that is neither a harness nor retired is still an
/// [`UnknownHarness`], so a typo or a foreign file does not read as history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordedHarness {
    Known(Harness),
    /// A [`RETIRED`] entry, by its wire spelling.
    Retired(&'static str),
}

impl RecordedHarness {
    /// The harness this build can act on, or `None` for a retired one.
    pub const fn known(self) -> Option<Harness> {
        match self {
            RecordedHarness::Known(h) => Some(h),
            RecordedHarness::Retired(_) => None,
        }
    }

    /// The wire spelling, exactly as the file wrote it.
    pub const fn as_str(self) -> &'static str {
        match self {
            RecordedHarness::Known(h) => h.as_str(),
            RecordedHarness::Retired(s) => s,
        }
    }

    /// The name a person reads: [`Harness::cli_name`], and `retired harness (<name>)` for a retired
    /// one, so no view presents it as a harness it could still run.
    pub fn cli_name(self) -> String {
        match self {
            RecordedHarness::Known(h) => h.cli_name().to_string(),
            RecordedHarness::Retired(s) => format!("retired harness ({s})"),
        }
    }
}

impl From<Harness> for RecordedHarness {
    fn from(h: Harness) -> Self {
        RecordedHarness::Known(h)
    }
}

impl PartialEq<Harness> for RecordedHarness {
    fn eq(&self, other: &Harness) -> bool {
        self.known() == Some(*other)
    }
}

impl fmt::Display for RecordedHarness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RecordedHarness {
    type Err = UnknownHarness;

    /// [`Harness::from_str`], then [`RETIRED`], and nothing else.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.parse::<Harness>() {
            Ok(h) => Ok(RecordedHarness::Known(h)),
            Err(e) => retired(s).map(RecordedHarness::Retired).ok_or(e),
        }
    }
}

/// The bare wire string, as [`Harness`] writes it: a known harness's file is byte-for-byte what it
/// was before this type existed.
impl Serialize for RecordedHarness {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RecordedHarness {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
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

    /// **A recorded harness reads a retired name, and only a retired one.** Every harness and every
    /// [`RETIRED`] entry round-trips through its wire string; a name that is neither is still the
    /// same [`UnknownHarness`] a `Harness` refuses it with.
    #[test]
    fn a_recorded_harness_keeps_a_retired_name_and_refuses_an_unknown_one() {
        for h in Harness::ALL {
            let r: RecordedHarness = serde_json::from_str(&format!("\"{h}\"")).unwrap();
            assert_eq!(r, RecordedHarness::Known(h));
            assert_eq!(serde_json::to_string(&r).unwrap(), format!("\"{h}\""));
        }
        for name in RETIRED {
            let r: RecordedHarness = serde_json::from_str(&format!("\"{name}\"")).unwrap();
            assert_eq!(r, RecordedHarness::Retired(name));
            assert_eq!(r.known(), None, "nothing can act on a retired harness");
            assert_eq!(r.cli_name(), format!("retired harness ({name})"));
            assert_eq!(serde_json::to_string(&r).unwrap(), format!("\"{name}\""));
            assert!(name.parse::<Harness>().is_err(), "{name} is no harness");
        }
        assert_eq!(
            "bard".parse::<RecordedHarness>(),
            Err(UnknownHarness("bard".into()))
        );
        let e = serde_json::from_str::<RecordedHarness>("\"bard\"").unwrap_err();
        assert!(e.to_string().contains("unknown harness"), "{e}");
        assert_eq!(
            "claude".parse::<RecordedHarness>(),
            Ok(RecordedHarness::Known(Harness::ClaudeCode))
        );
    }
}
