//! The parts of design §4's event IR that the journal needs today: **§4.1 `Provenance`** and
//! **§4.2's `SrcSeq`**.
//!
//! The full `Event` of §4 — `global_seq`, `agent_seq`, `Payload` and the rest — lands with
//! `events.jsonl`. What is here is only what a journal record carries, because a journal record
//! makes the same two claims every IR record makes: *where did this come from*, and *what ordering
//! evidence does its source supply*. Splitting those two types out now, rather than inventing
//! journal-only spellings of them, is what keeps one answer when `Event` arrives.
//!
//! Pure data. Nothing here reads a file or a clock.

use serde::{Deserialize, Serialize};

/// A harness-native event identity — Claude Code's transcript `uuid`, Codex's `msg_09cb…`.
///
/// A newtype rather than a bare `String` because §4.2 gives it a *job*: it is the thing a
/// `Predecessor` names, and an id used as a chain link is not interchangeable with an id used as a
/// label.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EventId(pub String);

/// §4.2's **source-side ordering evidence, and it takes two forms** — because no day-one harness
/// emits a per-event ordinal on a surface marion can use.
///
/// | form | detects loss by | who supplies it |
/// |---|---|---|
/// | `Ordinal(u64)` | a gap in the sequence | opencode's `/api/event`, which §6.4 rejects |
/// | `Predecessor(EventId)` | a broken chain | Claude Code `TranscriptRecords`, via `parentUuid` |
/// | *absent* | not detectable | Codex app-server, Claude Code `headless` |
///
/// Typing this as a bare `u64` — the retracted shape §12 records — made the check unsatisfiable on
/// both day-one adapters, which is why it is an enum. **Where it is `None`, marion cannot detect
/// loss and must not imply otherwise.**
///
/// Externally tagged: `{"Ordinal":7}` / `{"Predecessor":"uuid"}`. Pinned by test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SrcSeq {
    Ordinal(u64),
    Predecessor(EventId),
}

/// Where a record came from. `Marion` is marion's own observation — which is what **every** journal
/// record is, since the journal records marion's own decisions rather than a harness's stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    Protocol,
    Transcript,
    Pty,
    Marion,
}

/// §4.1: what the UI keys on before claiming anything about loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Completeness {
    Complete,
    Partial,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transformation {
    Native,
    Normalized,
    Inferred,
}

/// §4.1. Recorded along several axes rather than ranked on one: a vendor transcript is exact but
/// delayed, which is not the same as lossy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub source: Source,
    /// `#[serde(default)]` throughout this type: §4.1 will gain axes, and a record written before
    /// one existed must still deserialize.
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub observed_live: bool,
    /// Is this the harness's system of record? What reconciliation keys on when the same fact
    /// arrives twice.
    #[serde(default)]
    pub authoritative: bool,
    #[serde(default = "Provenance::unknown_completeness")]
    pub completeness: Completeness,
    #[serde(default = "Provenance::native")]
    pub transformation: Transformation,
}

impl Provenance {
    fn unknown_completeness() -> Completeness {
        Completeness::Unknown
    }

    fn native() -> Transformation {
        Transformation::Native
    }

    /// marion's own act, observed by marion as it happened.
    ///
    /// `authoritative: true` is not self-flattery: for "marion decided to spawn this node" there is
    /// no other system of record — no harness knows marion made the decision. `Complete` for the
    /// same reason: the record is the whole of the fact, not a window onto a stream that may have
    /// dropped something.
    pub fn marion() -> Self {
        Self {
            source: Source::Marion,
            source_id: None,
            observed_live: true,
            authoritative: true,
            completeness: Completeness::Complete,
            transformation: Transformation::Native,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn src_seq_pins_the_two_forms_section_4_2_specifies() {
        // The retracted shape was a bare `u64`. If this ever serializes as a number again, the
        // `Predecessor` form — the only one that works on a day-one adapter — has been lost.
        assert_eq!(
            serde_json::to_string(&SrcSeq::Ordinal(7)).unwrap(),
            r#"{"Ordinal":7}"#
        );
        assert_eq!(
            serde_json::to_string(&SrcSeq::Predecessor(EventId("uuid-1".into()))).unwrap(),
            r#"{"Predecessor":"uuid-1"}"#
        );
        assert!(serde_json::from_str::<SrcSeq>("7").is_err());
    }

    #[test]
    fn src_seq_round_trips() {
        for s in [
            SrcSeq::Ordinal(0),
            SrcSeq::Ordinal(u64::MAX),
            SrcSeq::Predecessor(EventId("019-abc".into())),
        ] {
            let j = serde_json::to_string(&s).unwrap();
            assert_eq!(serde_json::from_str::<SrcSeq>(&j).unwrap(), s);
        }
    }

    #[test]
    fn provenance_pins_its_wire_shape() {
        assert_eq!(
            serde_json::to_value(Provenance::marion()).unwrap(),
            serde_json::json!({
                "source": "Marion",
                "source_id": null,
                "observed_live": true,
                "authoritative": true,
                "completeness": "Complete",
                "transformation": "Native",
            })
        );
    }

    #[test]
    fn a_provenance_written_before_a_field_existed_still_deserializes() {
        // The additive rule, exercised rather than asserted: the oldest conceivable shape is the
        // one required field alone.
        let old: Provenance = serde_json::from_str(r#"{"source":"Protocol"}"#).unwrap();
        assert_eq!(old.source, Source::Protocol);
        assert_eq!(
            old.completeness,
            Completeness::Unknown,
            "an absent completeness must not read as Complete — that would claim loss detection \
             the record never supported"
        );
        assert_eq!(old.transformation, Transformation::Native);
        assert!(!old.observed_live);
    }
}
