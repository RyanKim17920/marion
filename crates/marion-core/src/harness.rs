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
use std::sync::RwLock;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The harnesses marion knows how to name. Naming one is not the same as having an adapter for it
/// — `marion_harness::adapter_for` is where "known" narrows to "implemented".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// **A harness a row file names** (`marion_harness::row_file`): no variant of its own, its
    /// name interned once per process ([`Harness::named`]). One the journal names that this
    /// process has not loaded still replays — it is only not launchable ([`Harness::is_loaded`]).
    Named(NameId),
}

/// An interned row name ([`Harness::Named`]): an index into this process's append-only table of
/// names. Comparable only within one process; what is written anywhere is the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NameId(u32);

impl NameId {
    /// The name this id was interned for.
    fn name(self) -> &'static str {
        NAMES
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(self.0 as usize)
            .map_or("unknown", |n| n.name)
    }
}

/// One interned name, and whether a row by it is loaded.
struct Interned {
    name: &'static str,
    loaded: bool,
}

/// Every name interned by this process, in order: leaked once each, never removed, so a
/// [`NameId`] is valid for the process's life.
static NAMES: RwLock<Vec<Interned>> = RwLock::new(Vec::new());

/// **Whether `name` can be a row's name**: lowercase words of letters and digits joined by single
/// dashes, starting with a letter — the spelling of every built-in, and a name no argv or path can
/// bend.
pub fn valid_row_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Why a row's name cannot be loaded ([`Harness::load`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameRefused {
    #[error("`{0}` is not a row name: lowercase letters, digits and single dashes, from a letter")]
    Invalid(String),
    #[error("`{0}` is a harness marion ships; a row file may not shadow it")]
    BuiltIn(String),
    #[error("`{0}` names a harness this build has retired")]
    Retired(String),
    #[error("a row named `{0}` is already loaded")]
    Loaded(String),
}

impl Harness {
    /// The wire spelling. The one place the strings live.
    pub fn as_str(self) -> &'static str {
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
            Harness::Named(id) => id.name(),
        }
    }

    /// The name a person types and reads: [`Self::as_str`], except `claude` for Claude Code, the
    /// spelling `marion claude` and the agent types use. Output meant for an operator (doctor's
    /// rows) says this; the wire, the journal and contracts keep [`Self::as_str`].
    pub fn cli_name(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude",
            other => other.as_str(),
        }
    }

    /// Every harness marion ships — the built-ins. A loaded row's is [`Self::loaded`]; both are
    /// `marion_harness::adapter::every`.
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

    /// **The harness `name` names**: a built-in by its wire or CLI spelling, else the interned
    /// [`Harness::Named`] — loaded or not. `None` for a name no row could have.
    pub fn named(name: &str) -> Option<Harness> {
        if let Some(h) = builtin(name) {
            return Some(h);
        }
        if !valid_row_name(name) {
            return None;
        }
        let mut names = NAMES.write().unwrap_or_else(|e| e.into_inner());
        let at = match names.iter().position(|n| n.name == name) {
            Some(at) => at,
            None => {
                names.push(Interned {
                    name: Box::leak(name.to_owned().into_boxed_str()),
                    loaded: false,
                });
                names.len() - 1
            }
        };
        Some(Harness::Named(NameId(u32::try_from(at).ok()?)))
    }

    /// **Mark a row by `name` loaded**, and answer its harness. Refuses a name a built-in (or its
    /// CLI spelling) has, a retired one, an invalid one, and one already loaded: two files may not
    /// both define a harness.
    pub fn load(name: &str) -> Result<Harness, NameRefused> {
        if builtin(name).is_some() {
            return Err(NameRefused::BuiltIn(name.into()));
        }
        if RETIRED.contains(&name) {
            return Err(NameRefused::Retired(name.into()));
        }
        let Some(Harness::Named(NameId(at))) = Harness::named(name) else {
            return Err(NameRefused::Invalid(name.into()));
        };
        let mut names = NAMES.write().unwrap_or_else(|e| e.into_inner());
        let entry = &mut names[at as usize];
        if entry.loaded {
            return Err(NameRefused::Loaded(name.into()));
        }
        entry.loaded = true;
        Ok(Harness::Named(NameId(at)))
    }

    /// Whether a node of this harness can be launched by this process: every built-in, and a
    /// named one whose row is loaded.
    pub fn is_loaded(self) -> bool {
        match self {
            Harness::Named(NameId(at)) => NAMES
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(at as usize)
                .is_some_and(|n| n.loaded),
            _ => true,
        }
    }

    /// The named harnesses this process has loaded rows for, by name.
    pub fn loaded() -> Vec<Harness> {
        let names = NAMES.read().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<(&'static str, Harness)> = names
            .iter()
            .enumerate()
            .filter(|(_, n)| n.loaded)
            .filter_map(|(at, n)| Some((n.name, Harness::Named(NameId(u32::try_from(at).ok()?)))))
            .collect();
        out.sort_by_key(|(name, _)| *name);
        out.into_iter().map(|(_, h)| h).collect()
    }

    /// Whether this harness is one marion ships, as opposed to one a row file names.
    pub fn is_builtin(self) -> bool {
        !matches!(self, Harness::Named(_))
    }

    /// This built-in's place in [`Harness::ALL`], or `None` for a named harness.
    pub fn builtin_index(self) -> Option<usize> {
        Harness::ALL.iter().position(|h| *h == self)
    }
}

/// Ordered by name, so an order is the same in every process whatever was interned first.
impl PartialOrd for Harness {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Harness {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

/// The built-in `s` names — its wire spelling, or `claude` for Claude Code.
fn builtin(s: &str) -> Option<Harness> {
    if s == "claude" {
        return Some(Harness::ClaudeCode);
    }
    Harness::ALL.into_iter().find(|h| h.as_str() == s)
}

/// **Wire spellings of harnesses marion once launched and no longer has a row for.**
///
/// Not a harness: nothing parses to one, and an agent type naming one is refused like any other
/// unknown name. It exists for the journal, which is append-only and outlives a retirement — a
/// record written while the harness was supported still names it, and replay must keep that
/// record rather than stop at it ([`crate::journal::decode`]). A row file may not take one of
/// these names ([`Harness::load`]).
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
    pub fn as_str(self) -> &'static str {
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
     qwen, agy, pi, acp (claude-code is accepted for claude){loaded}",
    loaded = loaded_names()
)]
pub struct UnknownHarness(pub String);

/// The loaded row names, for [`UnknownHarness`]'s sentence: `, and the loaded rows x, y` or
/// nothing.
fn loaded_names() -> String {
    let names: Vec<&str> = Harness::loaded().into_iter().map(Harness::as_str).collect();
    if names.is_empty() {
        String::new()
    } else {
        format!(", and the loaded rows {}", names.join(", "))
    }
}

/// **Strict**: a built-in, or a named harness whose row this process has loaded — what an agent
/// type's `harness` key and a command line are parsed with.
impl FromStr for Harness {
    type Err = UnknownHarness;

    /// The wire spelling, plus `claude` for Claude Code: the name a person types everywhere else
    /// (agent types, `marion claude`), accepted as input while the wire keeps `claude-code`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(h) = builtin(s) {
            return Ok(h);
        }
        let loaded = Harness::loaded().into_iter().find(|h| h.as_str() == s);
        loaded.ok_or_else(|| UnknownHarness(s.to_string()))
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

/// **Lenient**: a built-in, or any row name, loaded or not — a journal a process with other rows
/// wrote still replays, and a node of an unloaded row is only refused a launch. A retired name is
/// still refused, so the journal reads that record as [`crate::journal::RecordKind::
/// RetiredHarness`].
impl<'de> Deserialize<'de> for Harness {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if RETIRED.contains(&s.as_str()) {
            return Err(serde::de::Error::custom(UnknownHarness(s)));
        }
        Harness::named(&s).ok_or_else(|| serde::de::Error::custom(UnknownHarness(s)))
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

    /// **A named harness round-trips through its name, loaded or not**: the journal of a process
    /// that loaded a row replays in one that did not, and only a launch needs it loaded. A strict
    /// parse accepts it once loaded; a retired name is refused either way.
    #[test]
    fn a_named_harness_round_trips_through_its_name_and_parses_once_loaded() {
        let replayed: Harness = serde_json::from_str("\"tb8-replayed\"").unwrap();
        assert!(!replayed.is_builtin() && !replayed.is_loaded());
        assert_eq!(
            serde_json::to_string(&replayed).unwrap(),
            "\"tb8-replayed\""
        );
        assert_eq!(replayed.as_str(), "tb8-replayed");
        assert!(
            "tb8-replayed".parse::<Harness>().is_err(),
            "not loaded, not parsed"
        );
        assert!(
            serde_json::from_str::<Harness>("\"gemini\"").is_err(),
            "retired"
        );

        let loaded = Harness::load("tb8-loaded").unwrap();
        assert!(loaded.is_loaded());
        assert_eq!("tb8-loaded".parse::<Harness>(), Ok(loaded));
        assert_eq!(
            Harness::named("tb8-loaded"),
            Some(loaded),
            "one id per name"
        );
        assert!(Harness::loaded().contains(&loaded));
        assert!(
            "tb8-nope"
                .parse::<Harness>()
                .unwrap_err()
                .to_string()
                .contains("tb8-loaded"),
            "the error names the loaded rows"
        );
    }

    /// **A row may not take a name marion ships, one it retired, one already loaded, or one no row
    /// could have.**
    #[test]
    fn a_row_name_that_shadows_or_bends_is_refused() {
        for (name, refused) in [
            ("codex", NameRefused::BuiltIn("codex".into())),
            ("claude", NameRefused::BuiltIn("claude".into())),
            ("gemini", NameRefused::Retired("gemini".into())),
            ("Goose2", NameRefused::Invalid("Goose2".into())),
            ("../x", NameRefused::Invalid("../x".into())),
            ("a--b", NameRefused::Invalid("a--b".into())),
        ] {
            assert_eq!(Harness::load(name), Err(refused), "{name}");
        }
        Harness::load("tb8-twice").unwrap();
        assert_eq!(
            Harness::load("tb8-twice"),
            Err(NameRefused::Loaded("tb8-twice".into()))
        );
    }

    /// **Order is by name**, so it cannot depend on which process interned what first.
    #[test]
    fn harnesses_order_by_name_whatever_was_interned_first() {
        let z = Harness::named("tb8-zz").unwrap();
        let a = Harness::named("tb8-aa").unwrap();
        assert!(a < z);
        assert!(Harness::Acp < a && Harness::Codex < Harness::Goose);
    }
}
