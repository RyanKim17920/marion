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
    /// GitHub Copilot CLI, headless `-p` (measured on 1.0.83, `tests/fixtures/s24/`). The sixth
    /// name and the fifth binary; added after `Acp`'s doc comment below was written, so where that
    /// comment says "the other four" read "the other five".
    Copilot,
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
            Harness::Gemini => "gemini",
            Harness::OpenCode => "opencode",
            Harness::Copilot => "copilot",
            Harness::Acp => "acp",
        }
    }

    /// **Can a node on this harness change a file with an empty `tools:` list?**
    ///
    /// §6.6's occupancy rule is *"at most one node with **write tools** per cwd"*, and reading that
    /// off [`crate::agent_type::AgentType::tools`] alone gets the answer backwards on half the
    /// harnesses. §3.1's `tools:` is a **grant list** — what marion must positively enable that the
    /// harness would not do on its own — and two of the four need no such grant. That asymmetry is
    /// already stated in [`crate::agent_type::TOOL_READ`]'s table, which says outright that
    /// answering `read` with codex's shell *"would let a reader of `tools: [read]` believe a codex
    /// node was read-only when it is not"*. The same trap, one field over.
    ///
    /// What each adapter actually compiles, which is where these two answers come from:
    ///
    /// | harness | with `tools: []` | source |
    /// |---|---|---|
    /// | codex | **writes** — `sandbox_mode = "workspace-write"` on every node marion configures, and `codex exec` has no per-tool knob at all | `codex::SANDBOX_MODE` |
    /// | opencode | **writes** — marion compiles no constraint whatsoever | `opencode::NO_COMPILED_TOOL_CONSTRAINT` |
    /// | claude-code | withheld — `--tools ""` unless `write` is declared | `ClaudeCodeAdapter::permission_axis` |
    /// | gemini | withheld — the default approval mode drops the mutating tools from `functionDeclarations` outright | `gemini::DEFAULT_APPROVAL_MODE` |
    /// | copilot | withheld — `--available-tools` names only marion's verbs unless `write` is declared, and an ungranted `create` is `denied` (measured, `tests/fixtures/s24/`) | `CopilotAdapter::permission_axis` |
    /// | acp | **writes** — twice over; see below | `acp::NO_TOOL_AVAILABILITY_SURFACE` |
    ///
    /// **The `acp` arm is decided, not inherited.** Two independent reasons, either sufficient:
    ///
    /// 1. *ACP has no tool-availability surface at all.* There is no field in `initialize` or
    ///    `session/new` that narrows an agent's own tools, so marion compiles no constraint — the
    ///    opencode row's situation, one protocol up. S21 measured it: an `opencode acp` session
    ///    marion opened had `write`, `edit` and `bash` in scope with marion having asked for
    ///    nothing. Worse, marion's own `initialize` **hands the agent a write channel**
    ///    (`clientCapabilities.fs.writeTextFile: true`, [`crate`-external
    ///    `marion_harness::acp::initialize_request`]), so an ACP node with `tools: []` can change a
    ///    file *through marion*.
    /// 2. *marion does not know which agent this is until after the process exists.* §5.2's `acp`
    ///    row is one adapter over many agents and the identity arrives in the handshake, so any
    ///    `false` here would be a claim about an agent nobody had named yet.
    ///
    /// Both land on the same answer as the erring-towards-`true` rule below, which is the only
    /// reason a fifth arm is safe to add at all.
    ///
    /// **Erring towards `true` is the only safe direction here** and is what the two `true` arms
    /// are. A false `true` costs a caller a refusal they can lift with one documented parameter
    /// (`allow_concurrent_writes`); a false `false` lets two agents write one tree, which §6.6
    /// calls worse than two humans doing it — each harness keeps its own checkpoint state, so a
    /// restore in one silently reverts the other's work — and the caller finds out afterwards, if
    /// at all.
    ///
    /// Exhaustive rather than `_ => true`, so a sixth harness cannot inherit an answer nobody
    /// measured for it. It lives in `marion-core` because §6.6's rule does, and the adapters that
    /// own the evidence pin it from their side — see `adapter.rs`'s
    /// `the_harnesses_that_write_without_a_grant_are_the_ones_that_compile_no_constraint`.
    pub const fn writes_without_a_declaration(self) -> bool {
        match self {
            Harness::Codex | Harness::OpenCode | Harness::Acp => true,
            Harness::ClaudeCode | Harness::Gemini | Harness::Copilot => false,
        }
    }

    /// Every harness marion can name — what `marion doctor` would list.
    pub const ALL: [Harness; 6] = [
        Harness::ClaudeCode,
        Harness::Codex,
        Harness::Gemini,
        Harness::OpenCode,
        Harness::Copilot,
        Harness::Acp,
    ];
}

/// An unrecognised `harness:` value. §3.1: unknown keys are a **load error**, never a silent
/// default — defaulting would compile some other harness's argv for a type that asked for one
/// marion has never heard of.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "unknown harness {0:?}; known harnesses are claude-code, codex, gemini, opencode, copilot, acp"
)]
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
        assert_eq!(Harness::Copilot.as_str(), "copilot");
        // §5.2's adapter row is spelled `acp`, and it names the protocol rather than an agent.
        assert_eq!(Harness::Acp.as_str(), "acp");
    }

    /// The six answers, named one at a time. A `_ => true` arm, or a seventh harness copying a
    /// neighbour, changes exactly one of these rows.
    #[test]
    fn each_harness_answers_the_occupancy_question_for_itself() {
        assert_eq!(
            Harness::ALL.map(|h| (h.as_str(), h.writes_without_a_declaration())),
            [
                ("claude-code", false),
                ("codex", true),
                ("gemini", false),
                ("opencode", true),
                // Measured on 1.0.83 (`tests/fixtures/s24/`): `--available-tools` withholds every
                // built-in it does not name, and an ungranted `create` is denied at exit 0.
                ("copilot", false),
                ("acp", true),
            ]
        );
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
