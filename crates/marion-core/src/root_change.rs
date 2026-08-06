//! **The root change record** — what a root did to the operator's own repository (design §6.7, §9).
//!
//! A child runs in a git worktree marion created, and §6.7 derives `changed_paths`,
//! `scope_violations` and `diff` from a diff of it. A **root** runs in `RootSpec::repo`, the
//! operator's live checkout, and has no `TaskContract` to record any of that in (§9). Until this
//! module existed a root therefore produced *no record of what it did at all* — byte-identical, to
//! a reader, to the signature §11 item 24 describes and commit `8a69f22` exists to tell apart: an
//! empty `changed_paths` beside a node that really did write. That ambiguity is the whole reason
//! this file is here.
//!
//! # Two halves, because 16 KiB is an atomicity constant and not a budget
//!
//! [`crate::journal::MAX_RECORD_BYTES`] is the size a journal record must stay under for one
//! `O_APPEND` `write(2)` to be atomic against a concurrent writer — that is what replaces a lock
//! between `marion run` and each `marion-supervisor mcp` bridge. It is not negotiable, and it does
//! not merely make a *diff* too big to journal: it makes a **path list** too big too. A root that
//! runs a codemod over 5,000 files would produce a record refused at encode time
//! (`journal.rs:283`), and refused *silently*, because `journal::record` reports to stderr and
//! carries on (`supervisor/journal.rs:179-183`). A record whose size depends on what the root did
//! fails hardest exactly where the audit matters most.
//!
//! So the record is **O(1) in what the root did**, and the unbounded half lives in a file:
//!
//! - [`RootChanged`] — the journal's half. Ids, oids and `usize`s, nothing else. No `PathBuf`, no
//!   diff, and **no path to the sidecar**, because the leaf name is a constant
//!   ([`crate::paths::AgentDir::root_change`]) derivable from the `AgentId`.
//! - [`RootChange`] — the sidecar at `<agent-dir>/root-change.json`, authoritative for the paths
//!   and the patch.
//!
//! That split is not invented here. It is exactly `ContractPersisted` +
//! `contracts/<task_id>.json`, already normative at `journal.rs:127-130`: *"The journal records
//! that a contract exists and how it ended, never its contents: the file is the contract."*
//!
//! **The counts are derived, never stored twice.** [`RootChange::record`] computes the journal half
//! from the sidecar, so `changed_count` cannot disagree with `changed_paths.len()` — the failure a
//! second stored copy invites, and the one an auditor could never detect from either half alone.
//!
//! # What this record does not claim
//!
//! M1 has no filesystem attribution, and there is no honest way to separate the operator's
//! keystrokes from the agent's writes inside one directory. Hence [`RootChange::changed_paths`] is
//! *everything that changed in that directory between launch and exit, whoever did it* — the same
//! discipline `Completion::scope_enforced` holds to. A field named `root_writes` would claim a
//! measurement marion did not make. See §11 item 26 for what the delta cannot see at all.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::contract::{AgentId, Capped, Glob, Oid};

/// The byte cap on every free-text `reason` in this module.
///
/// The precedent is `spawn.rs:373`, the stderr preview marion puts in a `ProcessExit.description`.
/// The value is a *byte* count rather than a character count because what it protects is
/// [`crate::journal::MAX_RECORD_BYTES`], which is bytes.
pub const REASON_CAP: usize = 512;

/// A free-text explanation that **cannot be longer than [`REASON_CAP`] once constructed**.
///
/// The field is private and the only constructor truncates, which is what makes
/// [`RootChanged`]'s size independent of its input *by construction* rather than by the caller
/// remembering. A `pub reason: String` would put the journal's atomicity invariant in the hands of
/// every call site that ever formats an error message into it — and the failure would be silent
/// (`supervisor/journal.rs:179-183`), which is the failure class this whole module exists to end.
///
/// **Deserialization does not truncate, deliberately.** Replay reports what the journal says; a
/// reader that shortened an over-long value it found on disk would be narrating a record nobody
/// wrote. The cap belongs at the point bytes are *created*, and that is [`Reason::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Reason(String);

impl Reason {
    /// Truncate to [`REASON_CAP`] bytes, on a character boundary.
    ///
    /// Whole characters, not bytes: a `String` cut mid-UTF-8 is not representable, and the
    /// alternative — `chars().take(n)` — bounds characters rather than the bytes the journal cap
    /// is stated in, so a reason of 512 four-byte characters would be 2 KiB.
    pub fn new(s: impl Into<String>) -> Self {
        let mut s = s.into();
        if s.len() > REASON_CAP {
            let mut i = REASON_CAP;
            while i > 0 && !s.is_char_boundary(i) {
                i -= 1;
            }
            s.truncate(i);
        }
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What marion knows about the root's effect on the repository — **the journal's half**.
///
/// Every field is an id, a 40-hex oid, or a `usize`. That is §6.7 cap rule 6's *"size does not
/// depend on the input"*, held by construction here rather than by an algorithm applied afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootChanged {
    pub agent_id: AgentId,
    /// `HEAD` when the root was prepared — **context, and never the diff base.**
    ///
    /// A dirty repo makes `HEAD` attribute the operator's own uncommitted work to the root, which
    /// is a lie in an audit record and the exact failure class `8a69f22` ended (a clean bill of
    /// health that was not measured). The base is [`RootDelta::Observed::pre_tree`]. This field
    /// only says which commit the operator happened to be on.
    ///
    /// `None` when marion never got to read one — a directory that is not a git worktree, or a
    /// `git` that is not on `PATH`. An absence, not a zero oid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<Oid>,
    /// `HEAD` at exit. Different from `base_commit` means *something was committed during the run*
    /// — it does not claim the root did it, for the same reason `changed_paths` does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_at_exit: Option<Oid>,
    pub observation: RootObservation,
}

/// Whether marion looked, and what came of looking.
///
/// **The three readings this type exists to keep apart** are `None` (the journal says nothing),
/// `Some(NotAttempted{..})` (marion did not look, here is why) and `Some(Observed{changed_count:
/// 0})` (marion looked and nothing changed). Before this type the last two were the same bytes —
/// no record, or a record of nothing — and that is precisely the ambiguity `8a69f22` was written
/// to destroy for children.
///
/// The measurements live **inside** `Observed` and nowhere else. Hoisting them into [`RootChanged`]
/// would force a `pre_tree: Oid("")` and a `changed_count: 0` onto a record where marion measured
/// nothing at all, so `NotAttempted` and a clean `Observed` would carry identical numbers and
/// differ only by a tag — a field claiming a measurement marion did not make.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RootObservation {
    /// Both snapshots were taken. The delta between the two trees is the record.
    Observed {
        /// The working tree as a git tree object, at `prepare`.
        pre_tree: Oid,
        /// …and at exit.
        post_tree: Oid,
        /// Paths differing between the two trees. **Not "paths the root wrote"** — see
        /// [`RootChange::changed_paths`].
        changed_count: usize,
        /// Paths already differing from `base_commit` when the root launched. The operator's own
        /// work in progress, counted so a reader can see it was subtracted rather than assume it.
        dirty_at_launch: usize,
        /// Pre-cap byte length of the patch. 0 iff the trees are identical.
        diff_bytes: usize,
        scope_violation_count: usize,
    },
    /// marion did not look, and why. **The variant that makes an absence legible**: a run that
    /// produced no delta because nobody measured one must never read as a run that changed nothing.
    NotAttempted { reason: Reason },
    /// marion looked and could not see — `git` failed, the object dir was unwritable. Distinct from
    /// [`Self::NotAttempted`] because a decision not to measure and a measurement that broke send a
    /// reader to different fixes.
    Failed { reason: Reason },
}

/// The scope a root's writes are judged against (§5.4).
///
/// A child's writable scope is a **conjunction** of the agent type's ceiling and the `spawn`
/// request (`scope.rs:57-68`). A root has a ceiling (`agent_type.rs:107`) and **no request**,
/// because no parent authored one: the operator chose the directory by pointing `marion run` at
/// it, which is a different act with a different meaning. Encoding "no request" as `["**"]` would
/// fabricate an author, so the shape says so instead — and a reader can never mistake a root's
/// one-list check for a child's two-list one.
///
/// Every built-in defaults its ceiling to `["**"]` (`agent_type.rs:126`), so this yields zero
/// violations today and becomes meaningful the day a type states a ceiling. Enforcement is
/// **detective and never preventive**, exactly as it already is for children.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RootScope {
    CeilingOnly {
        ceiling: Vec<Glob>,
    },
    /// No scope judgement was made. Never means "no violation" — the same distinction
    /// `Completion::scope_enforced` carries.
    NotEnforced {
        reason: Reason,
    },
}

/// The delta itself: the half whose size depends on what the root did.
///
/// This is what makes [`RootChanged`] derivable rather than separately assembled. The counts in
/// [`RootObservation::Observed`] are computed from these lists by [`RootChange::record`], so the
/// journal cannot claim a number the file contradicts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RootDelta {
    Observed {
        pre_tree: Oid,
        post_tree: Oid,
        /// Everything that differs between `pre_tree` and `post_tree`, in `git diff --name-only
        /// --no-renames` order.
        changed_paths: Vec<PathBuf>,
        /// Paths already differing from `base_commit` at launch — the operator's uncommitted work.
        /// Recorded so a reader can see what the tree base subtracted, which a path-list base point
        /// could only approximate.
        pre_dirty_paths: Vec<PathBuf>,
        /// Paths in `changed_paths` failing [`RootScope`]. Derived from the **full** list before
        /// any cap, so no elision can hide a violation — §6.7's rule for `Completion`, unchanged.
        scope_violations: Vec<PathBuf>,
        /// The patch, `Capped` because a consumer must be able to tell a complete value from a
        /// prefix (`contract.rs:13-24`). One field and not a second `.diff` file: two files would
        /// be two sources of truth for the same bytes.
        diff: Option<Capped<String>>,
    },
    NotAttempted {
        reason: Reason,
    },
    Failed {
        reason: Reason,
    },
}

/// `<agent-dir>/root-change.json` — **authoritative for the paths and the patch**.
///
/// The journal points at a node; this file says what happened to the tree. Same relationship
/// `contracts/<task_id>.json` has to [`crate::journal::ContractPersisted`], and for the same
/// reason: elision is only safe when something behind it is complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootChange {
    pub agent_id: AgentId,
    /// See [`RootChanged::base_commit`] — context, never the diff base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<Oid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_at_exit: Option<Oid>,
    pub scope: RootScope,
    /// **Everything that changed in the root's directory between launch and exit, whoever did it.**
    ///
    /// Named `working_tree_delta` and not `root_writes` because M1 has no filesystem attribution:
    /// an operator hand-edit, a background `cargo build` touching a tracked generated file, and a
    /// concurrent `marion run` all land in here. The name is the only place that honesty can live;
    /// nothing in the measurement can supply it.
    pub working_tree_delta: RootDelta,
}

impl RootChange {
    /// The journal's half of this record, **derived**.
    ///
    /// Never stored beside the lists it counts. A second copy is a second thing to keep in step,
    /// and the two halves of an audit record disagreeing is not a defect an auditor could catch
    /// from either half alone.
    pub fn record(&self) -> RootChanged {
        RootChanged {
            agent_id: self.agent_id.clone(),
            base_commit: self.base_commit.clone(),
            head_at_exit: self.head_at_exit.clone(),
            observation: match &self.working_tree_delta {
                RootDelta::Observed {
                    pre_tree,
                    post_tree,
                    changed_paths,
                    pre_dirty_paths,
                    scope_violations,
                    diff,
                } => RootObservation::Observed {
                    pre_tree: pre_tree.clone(),
                    post_tree: post_tree.clone(),
                    changed_count: changed_paths.len(),
                    dirty_at_launch: pre_dirty_paths.len(),
                    // The **pre-cap** length: the question a reader is asking is how big the root's
                    // patch was, not how much of it survived the cap.
                    diff_bytes: diff.as_ref().map(|d| d.original_bytes).unwrap_or(0),
                    scope_violation_count: scope_violations.len(),
                },
                RootDelta::NotAttempted { reason } => RootObservation::NotAttempted {
                    reason: reason.clone(),
                },
                RootDelta::Failed { reason } => RootObservation::Failed {
                    reason: reason.clone(),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::SystemTime;
    use crate::ir::Provenance;
    use crate::journal::{JournalRecord, MAX_RECORD_BYTES, RecordKind, WriterId, decode, encode};

    fn rec(kind: RecordKind) -> JournalRecord {
        JournalRecord {
            writer: WriterId("w-1".into()),
            seq: 0,
            ts: SystemTime::from_unix_millis(1_785_625_628_619),
            mono_ns: 1,
            provenance: Provenance::marion(),
            src_seq: None,
            kind,
        }
    }

    fn oid(s: &str) -> Oid {
        Oid(s.repeat(40)[..40].to_string())
    }

    fn observed(changed: usize) -> RootChanged {
        RootChanged {
            agent_id: AgentId("root".into()),
            base_commit: Some(oid("a")),
            head_at_exit: Some(oid("a")),
            observation: RootObservation::Observed {
                pre_tree: oid("b"),
                post_tree: oid("c"),
                changed_count: changed,
                dirty_at_launch: 0,
                diff_bytes: if changed == 0 { 0 } else { 120 },
                scope_violation_count: 0,
            },
        }
    }

    /// **NC-4, first half — the absence is recorded as an absence.**
    ///
    /// Asserted on the encoded bytes, not on the Rust value: what a future replayer, and any
    /// operator reading `journal.jsonl`, actually sees is the JSON. `8a69f22`'s whole subject is
    /// two situations that produced the same bytes.
    #[test]
    fn a_record_of_not_looking_is_not_the_bytes_of_a_record_of_seeing_nothing() {
        let looked = observed(0);
        let did_not = RootChanged {
            observation: RootObservation::NotAttempted {
                reason: Reason::new("the operator's directory is not a git worktree"),
            },
            head_at_exit: None,
            base_commit: None,
            ..observed(0)
        };
        assert_ne!(
            serde_json::to_value(&did_not).unwrap(),
            serde_json::to_value(&looked).unwrap(),
            "pre-8a69f22 these were the same bytes: an empty change list and no measurement at all"
        );
        // And the tag itself is what carries it, so a reader keying on the variant name is enough.
        let s = serde_json::to_string(&did_not).unwrap();
        assert!(s.contains("NotAttempted"), "{s}");
        assert!(
            s.contains("not a git worktree"),
            "an absence must say why, or it is only a shorter silence: {s}"
        );
        assert!(
            !s.contains("changed_count"),
            "a record where nothing was measured must carry no measurement: {s}"
        );
    }

    /// **NC-4, second half — `Failed` is not `NotAttempted`.** A decision not to measure and a
    /// measurement that broke send a reader to different fixes, so they must not encode alike.
    #[test]
    fn a_failed_look_is_distinguishable_from_one_never_attempted() {
        let why = "git write-tree: unable to write object";
        let failed = RootChanged {
            observation: RootObservation::Failed {
                reason: Reason::new(why),
            },
            ..observed(0)
        };
        let skipped = RootChanged {
            observation: RootObservation::NotAttempted {
                reason: Reason::new(why),
            },
            ..observed(0)
        };
        assert_ne!(
            serde_json::to_value(&failed).unwrap(),
            serde_json::to_value(&skipped).unwrap(),
            "same reason text, different events"
        );
    }

    /// **NC-7 — bounds. The direct test of "O(1) in what the root did".**
    ///
    /// A root that runs a codemod over 5,000 files is the case that decides this design: with a
    /// path list in the record it is refused at `journal.rs:283`, and refused *silently*
    /// (`supervisor/journal.rs:179-183`), so the loudest possible write produces no record at all.
    #[test]
    fn a_root_that_touched_five_thousand_files_still_produces_an_encodable_record() {
        let paths: Vec<PathBuf> = (0..5_000)
            .map(|i| PathBuf::from(format!("crates/marion-core/src/generated/module_{i}.rs")))
            .collect();
        let sidecar = RootChange {
            agent_id: AgentId("root".into()),
            base_commit: Some(oid("a")),
            head_at_exit: Some(oid("a")),
            scope: RootScope::CeilingOnly {
                ceiling: vec![Glob("**".into())],
            },
            working_tree_delta: RootDelta::Observed {
                pre_tree: oid("b"),
                post_tree: oid("c"),
                changed_paths: paths.clone(),
                pre_dirty_paths: vec![],
                scope_violations: paths.clone(),
                diff: Some(Capped::whole("x".repeat(4 * 1024 * 1024))),
            },
        };
        // The sidecar is unbounded on purpose — it is a file, not a `write(2)`.
        assert!(serde_json::to_vec(&sidecar).unwrap().len() > MAX_RECORD_BYTES);
        let line = encode(&rec(RecordKind::RootChanged(sidecar.record())))
            .expect("the journal's half is O(1) in what the root did");
        assert!(
            line.len() < MAX_RECORD_BYTES / 8,
            "a record of a 5,000-file codemod is {} bytes",
            line.len()
        );
        // **The O(1) claim itself, and not merely a small number.** A record of 5,000 paths and a
        // 4 MiB patch differs from a record of one path and a 7-byte patch only by the digits in
        // its counters — nothing in it is a function of the run's contents. A `changed_paths`
        // field would make this difference ~250 KB and the encode a silent refusal.
        let tiny = RootChange {
            working_tree_delta: RootDelta::Observed {
                pre_tree: oid("b"),
                post_tree: oid("c"),
                changed_paths: vec!["a.txt".into()],
                pre_dirty_paths: vec![],
                scope_violations: vec![],
                diff: Some(Capped::whole("+hi\n")),
            },
            ..sidecar.clone()
        };
        let small = encode(&rec(RecordKind::RootChanged(tiny.record()))).unwrap();
        assert!(
            line.len() - small.len() < 32,
            "the two records differ by {} bytes; only the counters' digits may differ",
            line.len() - small.len()
        );
        // …and it still carries the measurement, so the smallness is not emptiness.
        match sidecar.record().observation {
            RootObservation::Observed {
                changed_count,
                diff_bytes,
                scope_violation_count,
                ..
            } => {
                assert_eq!(changed_count, 5_000);
                assert_eq!(scope_violation_count, 5_000);
                assert_eq!(diff_bytes, 4 * 1024 * 1024);
            }
            other => panic!("{other:?}"),
        }
    }

    /// **NC-7 — a reason no caller bounded still fits, because the type bounded it.**
    #[test]
    fn a_hundred_kilobyte_reason_encodes_because_it_was_truncated_at_construction() {
        let r = RootChanged {
            observation: RootObservation::Failed {
                reason: Reason::new("x".repeat(100_000)),
            },
            ..observed(0)
        };
        assert_eq!(
            match &r.observation {
                RootObservation::Failed { reason } => reason.as_str().len(),
                _ => unreachable!(),
            },
            REASON_CAP
        );
        encode(&rec(RecordKind::RootChanged(r))).expect("truncated at construction, not at encode");
    }

    /// Truncation is by bytes and lands on a character boundary — a `String` cut mid-UTF-8 is not
    /// representable, and `chars().take(512)` would bound the wrong quantity by up to 4x.
    #[test]
    fn a_reason_is_capped_in_bytes_without_splitting_a_character() {
        let r = Reason::new("é".repeat(1_000));
        assert!(r.as_str().len() <= REASON_CAP);
        assert!(
            r.as_str().len() > REASON_CAP - 2,
            "it must take as much as fits, not stop at the first boundary"
        );
        assert_eq!(r.as_str().chars().count(), REASON_CAP / 2);
    }

    /// **NC-7 — the counters at their extreme.** `usize::MAX` is 20 digits; three of them plus two
    /// oids is still nowhere near the cap, which is the point: no input can move this number much.
    #[test]
    fn saturated_counters_still_encode_under_the_atomicity_cap() {
        let r = RootChanged {
            observation: RootObservation::Observed {
                pre_tree: oid("b"),
                post_tree: oid("c"),
                changed_count: usize::MAX,
                dirty_at_launch: usize::MAX,
                diff_bytes: usize::MAX,
                scope_violation_count: usize::MAX,
            },
            ..observed(0)
        };
        let line = encode(&rec(RecordKind::RootChanged(r))).unwrap();
        assert!(line.len() < MAX_RECORD_BYTES / 8, "{}", line.len());
    }

    /// The two halves cannot disagree, because one is computed from the other.
    #[test]
    fn the_journals_counts_are_the_sidecars_lengths_and_not_a_second_copy() {
        let sidecar = RootChange {
            agent_id: AgentId("root".into()),
            base_commit: None,
            head_at_exit: None,
            scope: RootScope::NotEnforced {
                reason: Reason::new("no ceiling could be resolved"),
            },
            working_tree_delta: RootDelta::Observed {
                pre_tree: oid("b"),
                post_tree: oid("c"),
                changed_paths: vec!["a.txt".into(), "b/c.rs".into()],
                pre_dirty_paths: vec!["operator.txt".into()],
                scope_violations: vec!["b/c.rs".into()],
                diff: Some(Capped::whole("+hello\n")),
            },
        };
        match sidecar.record().observation {
            RootObservation::Observed {
                changed_count,
                dirty_at_launch,
                diff_bytes,
                scope_violation_count,
                ..
            } => {
                assert_eq!(changed_count, 2);
                assert_eq!(dirty_at_launch, 1);
                assert_eq!(scope_violation_count, 1);
                assert_eq!(diff_bytes, 7);
            }
            other => panic!("{other:?}"),
        }
    }

    /// A capped diff still reports the length it *had*: the record answers "how big was the root's
    /// patch", and answering with the post-cap length would make a large change look small.
    #[test]
    fn diff_bytes_is_the_pre_cap_length() {
        let sidecar = RootChange {
            agent_id: AgentId("root".into()),
            base_commit: None,
            head_at_exit: None,
            scope: RootScope::CeilingOnly { ceiling: vec![] },
            working_tree_delta: RootDelta::Observed {
                pre_tree: oid("b"),
                post_tree: oid("c"),
                changed_paths: vec!["a.txt".into()],
                pre_dirty_paths: vec![],
                scope_violations: vec![],
                diff: Some(Capped {
                    value: "+he".into(),
                    truncated: true,
                    original_bytes: 900_000,
                }),
            },
        };
        match sidecar.record().observation {
            RootObservation::Observed { diff_bytes, .. } => assert_eq!(diff_bytes, 900_000),
            other => panic!("{other:?}"),
        }
    }

    /// The record round-trips through a journal line, like every other kind.
    #[test]
    fn the_record_round_trips_through_a_line() {
        for observation in [
            observed(3).observation,
            RootObservation::NotAttempted {
                reason: Reason::new("not a git worktree"),
            },
            RootObservation::Failed {
                reason: Reason::new("git: command not found"),
            },
        ] {
            let r = rec(RecordKind::RootChanged(RootChanged {
                observation,
                ..observed(0)
            }));
            let line = encode(&r).unwrap();
            assert_eq!(decode(&line[..line.len() - 1]).as_ref(), Some(&r));
        }
    }

    /// The sidecar round-trips as a document, since it is what a later reader opens.
    #[test]
    fn the_sidecar_round_trips_as_json() {
        let sidecar = RootChange {
            agent_id: AgentId("root".into()),
            base_commit: Some(oid("a")),
            head_at_exit: None,
            scope: RootScope::CeilingOnly {
                ceiling: vec![Glob("src/**".into())],
            },
            working_tree_delta: RootDelta::NotAttempted {
                reason: Reason::new("the operator passed --no-change-record"),
            },
        };
        let s = serde_json::to_string(&sidecar).unwrap();
        assert_eq!(serde_json::from_str::<RootChange>(&s).unwrap(), sidecar);
        assert!(
            s.contains("working_tree_delta"),
            "the field name carries the only attribution honesty there is: {s}"
        );
        assert!(
            !s.contains("root_writes"),
            "M1 has no filesystem attribution, so nothing may be named as if it had: {s}"
        );
    }
}
