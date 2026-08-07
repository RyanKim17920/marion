//! **§9's M2 criterion 3, second half: *"and no untracked live process."***
//!
//! `restart.rs` answers what the *journal* shows marion decided, and refuses to look at a process
//! at all — read its module doc, the refusal is the design. This module is the other half: given a
//! tree that `restart` has already judged, it asks the world whether any node marion is **not**
//! attached to still has a process running.
//!
//! # Why the answer has three values and not two
//!
//! The obvious shape is a predicate: *are there untracked live processes, yes or no?* Marion cannot
//! honestly compute that today, and a boolean would have to lie in one direction or the other.
//!
//! A pid alone answers *"is something wearing this number"*, which is not the question. The
//! question is *"is the process this journal is about still running"*, and after a supervisor crash
//! of unknown duration those differ: the kernel recycles pids. A boolean forces every ambiguous
//! node into `gone` (a false clean bill of health, which is the silent-degradation shape §12 exists
//! to refuse) or into `alive` (a false alarm that would have an operator hunting a process that
//! died last week).
//!
//! So the reading is [`Resolution`]: **alive-and-ours**, **gone**, or **cannot-tell** — and
//! cannot-tell is *named*, carries *why*, and **blocks the claim** rather than being quietly
//! counted as either neighbour. [`Audit::claim`] is correspondingly three-valued.
//!
//! # What makes a definite answer possible at all
//!
//! [`StartId`] — an opaque, platform-tagged process start identity, compared only for equality.
//! With one recorded beside the pid, three of the four cases become definite:
//!
//! | recorded | the kernel now says | resolution |
//! |---|---|---|
//! | any | no process wears that pid | **gone** |
//! | `Some(r)` | a process wears it, start id `r` | **alive and ours** |
//! | `Some(r)` | a process wears it, start id **not** `r` | **gone** — the pid was recycled |
//! | `None` | a process wears it | **cannot tell** |
//!
//! The last row is the whole of marion's remaining gap, and it is the row every node in a journal
//! written before `Spawned` carried a start id falls into.
//!
//! # The read is cheap, and that matters for where it can be called
//!
//! Measured on this platform: `sysctl(KERN_PROC_PID)` costs **15.2 µs**; `ps -o lstart=` costs
//! **4.35 ms and a fork** — about 285× more. A `ps` fork also sits badly with the boundary
//! `Cargo.toml` draws around `marion-testsupport` shelling out to `git` and `ps`: that is sanctioned
//! for a test helper and not for the spawn path. So this reads the kernel directly, through the
//! same `unsafe extern "C"` device `serve.rs` already uses for `getuid`/`getpeereid`.
//!
//! # `marion_testsupport::liveness` is a different question
//!
//! It reads `ps -o stat=` and answers alive/gone/zombie. That is *liveness*, and this module needs
//! *identity*; a zombie is alive to `kill(pid, 0)` and gone to anyone waiting for work to happen.
//! Neither answers "is this the same process". They are not interchangeable and neither replaces
//! the other.

use marion_core::contract::AgentId;
use marion_core::node::StartId;
use marion_core::registry::Replay;

/// What the kernel could tell marion about one pid.
///
/// Three values for the reason the module doc gives, and the third is not an error type: *"marion
/// could not ask"* is a fact about marion, and folding it into "no such process" is precisely the
/// false clean bill of health this module exists to refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    /// A process wears this pid and started with this identity.
    Id(StartId),
    /// The kernel says no process wears this pid. On macOS this is a **successful** `sysctl` that
    /// returned zero bytes, not an error — measured; a reader that only checked the return code
    /// would call a dead pid unreadable and lose the one definite answer available without a
    /// recorded identity.
    NoSuchProcess,
    /// marion could not ask, and says why in its own voice.
    Unavailable(String),
}

/// Why marion cannot say whether a node's process is still running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unprovable {
    /// A process wears the pid, but the journal recorded no [`StartId`] beside it, so marion cannot
    /// tell it from an unrelated process handed a recycled number. Every node journaled before
    /// `Spawned` carried a start id is this.
    NoRecordedIdentity,
    /// marion could not read a start identity at all — an unsupported platform, or a kernel that
    /// answered in a shape this build does not know.
    Unreadable(String),
}

/// Whether the process a node's journal names is still running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// A process wears the pid and carries the identity the journal recorded. This is an untracked
    /// live process: marion is not attached to it and it is still running.
    AliveAndOurs,
    /// Either nothing wears the pid, or what wears it is provably a different process.
    Gone,
    /// Named, and it blocks the claim. See [`Unprovable`].
    CannotTell(Unprovable),
}

/// **What marion's own record says about this node's process** — the half that decides whether a
/// live process is a leak or an expected survivor.
///
/// §9's *"no untracked live process"* is not *"no live process"*, and conflating them would make
/// the criterion unsatisfiable by design. §7.2 is explicit that `Orphaned` covers a process that
/// *"may be gone **or still running with marion no longer attached**"*, and a supervisor SIGKILL
/// leaves its children running on purpose — that is criterion 1. A running orphan is **on the
/// record**, enumerable, and offered to the operator to resolve.
///
/// What §11 item 30 actually calls an untracked live process is one *"reparented to pid 1, its
/// wall clock unenforced"* — a process nothing will ever attend to. §7.4 gives the same shape from
/// the other side (*"recoverable rather than producing an untracked live process"*), and §4.3's
/// note that *"losing a lifecycle record costs an untracked live process"* names the mechanism:
/// the record and the world disagree, so nobody is coming.
///
/// So the two standings are judged differently, and [`Audit::claim`] says how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// marion's record says this node is over — an observed exit, or a confirmed reap. If its
    /// process is nevertheless alive, **nothing in marion will ever attend to it**: no timeout is
    /// being enforced, no reaper is watching, and no restart will offer it for resolution, because
    /// as far as every reader of the journal is concerned it is finished. That is the leak.
    MarionSaysFinished,
    /// Marion is not attached, and **says so** — orphaned, an unresolved reap intent, an abort
    /// over a live spawn. A live process here is expected and tracked; §7.2 hands it to the
    /// operator.
    NotAttached,
}

/// One node with a process on the record, and what could be established about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub agent_id: AgentId,
    pub pid: i32,
    pub standing: Standing,
    pub resolution: Resolution,
}

impl Resolved {
    /// A live process that marion's record does not account for as live — §9's leak.
    pub fn is_untracked_and_live(&self) -> bool {
        self.resolution == Resolution::AliveAndOurs && self.standing == Standing::MarionSaysFinished
    }
}

/// §9's *"no untracked live process"*, as the three answers marion can actually give.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// Every process on the record was resolved definitely, and none of them is a process marion
    /// believes finished. The criterion holds.
    Holds,
    /// At least one process is running that marion's record says is finished. Nothing will attend
    /// to it — no timeout, no reaper, and no restart will offer it for resolution. The criterion
    /// **fails**, and these are the processes.
    Fails(Vec<Resolved>),
    /// No node was shown alive, but at least one could not be decided — so *"no untracked live
    /// process"* is unproven rather than true. **Never rounded up to [`Claim::Holds`]**: that
    /// rounding is the entire failure mode this type exists to prevent.
    CannotBeMade(Vec<Resolved>),
}

/// Every node marion is not attached to, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audit {
    pub resolved: Vec<Resolved>,
}

impl Audit {
    /// The nodes whose process is still running and is provably the one the journal names —
    /// **whatever their standing**. An expected survivor and a leak are both here; use
    /// [`Audit::untracked_and_live`] for the ones that are §9's failure.
    pub fn alive(&self) -> Vec<&Resolved> {
        self.filter(|r| matches!(r.resolution, Resolution::AliveAndOurs))
    }

    /// The leaks: alive, and marion's record says finished.
    pub fn untracked_and_live(&self) -> Vec<&Resolved> {
        self.filter(Resolved::is_untracked_and_live)
    }

    /// The nodes marion cannot decide, which are what stands between this and a claim.
    pub fn cannot_tell(&self) -> Vec<&Resolved> {
        self.filter(|r| matches!(r.resolution, Resolution::CannotTell(_)))
    }

    pub fn gone(&self) -> Vec<&Resolved> {
        self.filter(|r| matches!(r.resolution, Resolution::Gone))
    }

    fn filter(&self, f: impl Fn(&Resolved) -> bool) -> Vec<&Resolved> {
        self.resolved.iter().filter(|r| f(r)).collect()
    }

    /// **Leaks first, then undecided, then holds** — and the order is the correctness argument.
    ///
    /// A proven leak must be reported as one even if other nodes are undecided; answering
    /// `CannotBeMade` over it would downgrade a fact to an open question. Only a run with no leak
    /// *and* nothing undecided is a pass.
    ///
    /// Note what is **not** a failure: a node marion says it is *not attached to* whose process is
    /// alive. That is `Orphaned` working — see [`Standing`] — and counting it would mean the
    /// criterion could never hold after the very SIGKILL it is stated about.
    pub fn claim(&self) -> Claim {
        let leaked = self.untracked_and_live();
        if !leaked.is_empty() {
            return Claim::Fails(leaked.into_iter().cloned().collect());
        }
        let undecided = self.cannot_tell();
        if !undecided.is_empty() {
            return Claim::CannotBeMade(undecided.into_iter().cloned().collect());
        }
        Claim::Holds
    }
}

/// **The decision, as a pure function of what was recorded and what the kernel says.**
///
/// Separated from both the tree walk and the syscall so the whole truth table in the module doc is
/// unit-testable without a journal and without arranging for a real process to be in a particular
/// state. Every arm below is reachable from a test that passes values directly.
pub fn resolve(recorded: Option<&StartId>, now: Read) -> Resolution {
    match (recorded, now) {
        // Definite regardless of what was recorded: nothing is wearing the number, so whatever the
        // journal was about is not running.
        (_, Read::NoSuchProcess) => Resolution::Gone,
        // Definite in both directions, and this is what a recorded identity buys.
        (Some(was), Read::Id(is)) => {
            if *was == is {
                Resolution::AliveAndOurs
            } else {
                // A different process wears the number now, so marion's is gone. Reported as `Gone`
                // and not as a doubt: a mismatch is evidence, not absence of it.
                Resolution::Gone
            }
        }
        (None, Read::Id(_)) => Resolution::CannotTell(Unprovable::NoRecordedIdentity),
        (_, Read::Unavailable(why)) => Resolution::CannotTell(Unprovable::Unreadable(why)),
    }
}

/// Resolve **every node in a replayed tree that ever had a process**, and say where each stands.
///
/// In scope is exactly *"the journal records a pid for it"*. A node with no pid never reached
/// `command.spawn()`, so there is no process to account for; every other node has one that either
/// is or is not still running, and §9 wants both answered.
///
/// **Finished nodes are audited too, and that is the point rather than an oversight.** It would be
/// natural to look only at the nodes `restart` flags, since those are the ones marion is not
/// attached to — and it would miss the actual leak. A node marion recorded as exited or reaped is
/// one that *nothing will ever look at again*: its timeout is not being enforced and no restart
/// will offer it for resolution. If its process is somehow still alive, that is §11 item 30's
/// untracked live process exactly, and the only place it can be caught is here.
///
/// [`Standing`] comes from `restart::fate_decided` rather than being restated here, because a
/// second copy of that clause is how the two drift apart — `restart` makes the same argument about
/// its own `classify`.
pub fn audit(tree: &Replay) -> Audit {
    audit_with(tree, read)
}

/// [`audit`], with the kernel read injected — the seam the tests drive.
///
/// Present so a test can pin the tree-walking half (which nodes are in scope, which standing each
/// gets, how a mixture is reported) against every resolution, including ones that cannot be
/// arranged on demand from a test process. The real syscall is still measured directly, by
/// [`read`]'s own tests.
pub fn audit_with(tree: &Replay, probe: impl Fn(i32) -> Read) -> Audit {
    let mut resolved = Vec::new();
    for node in tree.nodes() {
        let Some(pid) = node.pid else {
            continue;
        };
        resolved.push(Resolved {
            // **Asked of the node, not derived from `restart::mark`'s output.** See
            // `restart::fate_decided`: reading "no marking" as "decided" is true before
            // `restart::apply` and false after it, because `apply` moves a node to `Orphaned` and
            // the unresolved clause then stops firing — so an audit run after the standard restart
            // pass, which is the only order §9's criterion is ever evaluated in, reported every
            // healthy orphan as a leak. Measured by the end-to-end test, not reasoned about.
            standing: if crate::restart::fate_decided(node) {
                Standing::MarionSaysFinished
            } else {
                Standing::NotAttached
            },
            resolution: resolve(recorded_start_id(node), probe(pid)),
            agent_id: node.agent_id.clone(),
            pid,
        });
    }
    Audit { resolved }
}

/// What the journal recorded as this node's start identity.
///
/// This used to be a hard-coded `None` with a doc explaining that it was the whole of marion's
/// remaining gap. `Spawned` carries one now, read inside `run.rs`'s `on_started` hook while marion
/// still holds the `Child` — the only instant at which the read is race-free by construction. It
/// stays a named function because `None` is still a real and correct answer: a journal written
/// before the field existed, a platform whose start-time read is unmeasured, or a root, whose
/// record is written after the reap and so has no pid to identify. All three mean the same thing to
/// a reader, which is that a live pid proves nothing about this node.
fn recorded_start_id(node: &marion_core::registry::ReplayedNode) -> Option<&StartId> {
    node.start_id.as_ref()
}

/// **The process start identity the kernel reports for `pid`, right now.**
///
/// See [`read_impl`] for the platform arm and what it was measured against.
pub fn read(pid: i32) -> Read {
    if pid <= 0 {
        // Not a process. `kill(0, …)` addresses the caller's whole process group and negative pids
        // address a group, so passing either to a per-process query would be asking a different
        // question than the caller thinks they asked.
        return Read::Unavailable(format!(
            "{pid} is not a process id — 0 names the caller's own process group and a negative \
             value names a group, and neither is a node's process"
        ));
    }
    read_impl(pid)
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn sysctl(
        name: *mut i32,
        namelen: u32,
        oldp: *mut core::ffi::c_void,
        oldlenp: *mut usize,
        newp: *mut core::ffi::c_void,
        newlen: usize,
    ) -> i32;
}

/// **macOS: `sysctl(CTL_KERN, KERN_PROC, KERN_PROC_PID, pid)`, and the first 16 bytes of what comes
/// back.**
///
/// Every constant here was measured on this platform rather than read off a header by eye:
///
/// * `offsetof(struct kinfo_proc, kp_proc.p_un.__p_starttime)` is **0** — the start time is a
///   union member at the very front of `struct extern_proc`, which is itself the first member of
///   `struct kinfo_proc` — and `sizeof(struct timeval)` is **16**. So the identity is the first 16
///   bytes and no struct definition has to be mirrored into Rust.
/// * `sizeof(struct kinfo_proc)` is **648**, and a buffer smaller than that fails with `ENOMEM`
///   rather than returning a truncated answer. The buffer here is deliberately larger, so a future
///   OS that grows the struct still answers; if one grows it past this, the call fails and that
///   becomes an honest [`Read::Unavailable`] rather than a wrong number.
/// * A pid with no process returns **success with `len == 0`**, not an error. Checked before the
///   length check, because it is the one definite answer available with no recorded identity.
/// * A **zombie** still answers. That matters for where the value is captured: it must be read
///   while marion still holds the `Child`, because after the reap the pid stops resolving.
///
/// The bytes are hex-encoded and tagged. Tagged because [`StartId`] is compared only for equality
/// and a journal carried to another platform must never produce a match; hex because the value is
/// opaque and a byte string with no interpretation is exactly what that means.
#[cfg(target_os = "macos")]
fn read_impl(pid: i32) -> Read {
    const CTL_KERN: i32 = 1;
    const KERN_PROC: i32 = 14;
    const KERN_PROC_PID: i32 = 1;
    /// Comfortably over the measured 648, so the struct can grow without this refusing.
    const CAP: usize = 4096;
    /// `sizeof(struct timeval)`, measured.
    const START: usize = 16;

    let mut mib = [CTL_KERN, KERN_PROC, KERN_PROC_PID, pid];
    let mut buf = [0u8; CAP];
    let mut len = CAP;
    // SAFETY: `mib` is four `i32`s and `namelen` says four; `buf` is `CAP` bytes and `len` says
    // `CAP`; `newp`/`newlen` are the documented "reading, not writing" pair. The call writes at
    // most `len` bytes and updates `len` to what it wrote.
    let rc = unsafe {
        sysctl(
            mib.as_mut_ptr(),
            4,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Read::Unavailable(format!(
            "the kernel refused to describe process {pid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    if len == 0 {
        return Read::NoSuchProcess;
    }
    if len < START {
        return Read::Unavailable(format!(
            "the kernel described process {pid} in {len} bytes, which is too few to hold the \
             start time this build reads — marion will not guess at an identity it cannot read"
        ));
    }
    let hex: String = buf[..START].iter().map(|b| format!("{b:02x}")).collect();
    Read::Id(StartId(format!("darwin-p_starttime:{hex}")))
}

/// **Every other platform: an explicit refusal, not a guess.**
///
/// The intended Linux arm is field 22 of `/proc/<pid>/stat` — the process start time in clock ticks
/// since boot. It is deliberately **not** implemented here, because nobody has run it. Everything
/// this module does with a [`StartId`] is decide whether a process is marion's, and a start-time
/// read that is subtly wrong — `/proc/<pid>/stat`'s second field is a `comm` that may itself
/// contain spaces and parentheses, so field 22 is not where a naive split puts it — turns
/// *"cannot tell"* into a confident wrong answer in both directions: marion reporting a stranger as
/// its own node, or a survivor as gone. Those are the two outcomes `restart.rs` refuses to
/// fabricate, and shipping an unmeasured parser here would reintroduce both under a different name.
///
/// So this refuses, the refusal is tested, and `cannot-tell` correctly blocks the claim on such a
/// platform. Implementing it is a measurement task, not a typing task: read the file for a known
/// pid, confirm the field against an independent source, and confirm a reaped pid stops resolving.
#[cfg(not(target_os = "macos"))]
fn read_impl(pid: i32) -> Read {
    Read::Unavailable(format!(
        "marion cannot read a process start identity on this platform, so it cannot tell whether \
         process {pid} is still the one its journal names. The Linux reading — field 22 of \
         /proc/<pid>/stat — is designed but unmeasured, and an unmeasured one would answer \
         confidently and sometimes wrongly. Until it is measured this is reported as `cannot tell`, \
         which blocks §9's `no untracked live process` rather than pretending to satisfy it."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::contract::AgentId;
    use marion_core::harness::Harness;
    use marion_core::ir::Provenance;
    use marion_core::journal::{JournalRecord, RecordKind, SpawnIntent, Spawned, WriterId};

    fn sid(s: &str) -> StartId {
        StartId(s.into())
    }

    /// **The whole truth table, as a table** — every row of the module doc, checked.
    ///
    /// Pure, so each row is a statement about the decision rather than about a process that had to
    /// be manoeuvred into a state. The two rows worth staring at are the last two: a *mismatch* is
    /// `Gone` because it is evidence (something else wears the number, so marion's is not running),
    /// while a *missing record* is `CannotTell` because it is the absence of evidence. Collapsing
    /// those two is the mistake this type exists to prevent.
    #[test]
    fn the_resolution_table_is_exactly_the_four_rows_and_a_mismatch_is_evidence() {
        let was = sid("darwin-p_starttime:aa");
        let other = sid("darwin-p_starttime:bb");

        assert_eq!(
            resolve(Some(&was), Read::NoSuchProcess),
            Resolution::Gone,
            "nothing wears the pid, so whatever the journal was about is not running"
        );
        assert_eq!(
            resolve(None, Read::NoSuchProcess),
            Resolution::Gone,
            "and that is definite even with nothing recorded — it is the one definite answer a \
             bare pid can still give, which is why the length check comes after it"
        );
        assert_eq!(
            resolve(Some(&was), Read::Id(was.clone())),
            Resolution::AliveAndOurs,
            "same pid, same start identity: this is an untracked live process"
        );
        assert_eq!(
            resolve(Some(&was), Read::Id(other)),
            Resolution::Gone,
            "a different process wears the number, so marion's is gone — a mismatch is evidence, \
             not doubt, and reporting `CannotTell` here would throw away the answer the recorded \
             identity was added to provide"
        );
        assert_eq!(
            resolve(None, Read::Id(was.clone())),
            Resolution::CannotTell(Unprovable::NoRecordedIdentity),
            "**marion's remaining gap.** Something is running under that pid and nothing on record \
             says whether it is this node"
        );
        assert!(
            matches!(
                resolve(Some(&was), Read::Unavailable("no".into())),
                Resolution::CannotTell(Unprovable::Unreadable(_))
            ),
            "and a read marion could not make is never silently a death"
        );
    }

    /// **A platform that cannot read a start identity blocks the claim rather than passing it.**
    ///
    /// This is the behaviour a non-macOS build has, tested on a platform where the arm itself
    /// cannot run: the arm's own sentence is unexercised here, but the thing that matters about it
    /// — that its `Unavailable` becomes `CannotTell` and stops `claim()` short of `Holds` — is
    /// exactly what this pins.
    #[test]
    fn a_platform_that_cannot_read_an_identity_cannot_claim_there_is_no_live_process() {
        let tree = tree_with_a_live_looking_node();
        let audit = audit_with(&tree, |_| Read::Unavailable("unmeasured platform".into()));
        assert!(
            matches!(audit.claim(), Claim::CannotBeMade(ref v) if v.len() == 1),
            "an unreadable identity is not a clean bill of health: {:?}",
            audit.claim()
        );
        assert!(audit.alive().is_empty() && audit.gone().is_empty());
    }

    /// One orphaned node with a pid, which is what a supervisor SIGKILL leaves behind.
    fn tree_with_a_live_looking_node() -> Replay {
        let mut r = Replay::default();
        for (seq, kind) in [
            RecordKind::SpawnIntent(SpawnIntent {
                agent_id: AgentId("019f0000-0000-7000-8000-00000000000a".into()),
                parent_id: None,
                agent_type: "claude".into(),
                harness: Harness::ClaudeCode,
                depth: 0,
                task_id: None,
            }),
            RecordKind::Spawned(Spawned {
                agent_id: AgentId("019f0000-0000-7000-8000-00000000000a".into()),
                harness_version: "x".into(),
                model: None,
                pid: Some(4242),
                start_id: None,
            }),
        ]
        .into_iter()
        .enumerate()
        {
            let rec = JournalRecord {
                writer: WriterId("w".into()),
                seq: seq as u64,
                ts: marion_core::encoding::SystemTime::from_unix_millis(
                    1_700_000_000_000 + seq as u64,
                ),
                mono_ns: seq as u64,
                provenance: Provenance::marion(),
                src_seq: None,
                kind,
            };
            r.extend(format!("{}\n", serde_json::to_string(&rec).unwrap()).as_bytes());
        }
        r
    }

    /// **Today, a supervisor SIGKILL leaves a node marion cannot decide** — the honest state of the
    /// recording, and the regression test the start-identity change has to flip.
    #[test]
    fn an_orphaned_node_with_a_live_pid_is_cannot_tell_while_no_identity_is_recorded() {
        let tree = tree_with_a_live_looking_node();
        assert_eq!(
            crate::restart::mark(&tree)
                .into_iter()
                .map(|m| m.marking)
                .collect::<Vec<_>>(),
            vec![crate::restart::Marking::Orphaned],
            "the premise: this is a node marion is not attached to"
        );
        let audit = audit_with(&tree, |_| Read::Id(sid("darwin-p_starttime:aa")));
        assert_eq!(audit.resolved.len(), 1);
        assert_eq!(
            audit.resolved[0].resolution,
            Resolution::CannotTell(Unprovable::NoRecordedIdentity),
            "`Spawned` carries no start identity, so a live pid proves nothing about this node"
        );
        assert!(
            matches!(audit.claim(), Claim::CannotBeMade(_)),
            "and §9's `no untracked live process` is therefore unproven, not true: {:?}",
            audit.claim()
        );
    }

    /// **A node marion says is finished, whose process is alive, is the leak — and an orphan whose
    /// process is alive is not.**
    ///
    /// This is the distinction §9 turns on, and getting it backwards breaks the criterion in both
    /// directions at once. §7.2 says an `Orphaned` node's process *"may be gone or still running
    /// with marion no longer attached"*, and a supervisor SIGKILL leaves children running by
    /// design — so counting a live orphan as a failure would make the criterion unsatisfiable by
    /// the very scenario it is stated about. Meanwhile a node marion recorded as *exited* is one
    /// nothing will ever look at again: no timeout enforced, no reaper, no restart offering it for
    /// resolution. §11 item 30 calls that a process *"reparented to pid 1, its wall clock
    /// unenforced"*, and this is the only place it can be caught.
    #[test]
    fn a_live_process_marion_believes_finished_is_the_leak_and_a_live_orphan_is_not() {
        let tree = tree_with_an_orphan_and_a_finished_node();
        let audit = audit_with(&tree, |_| Read::Id(sid("darwin-p_starttime:aa")));
        // Both nodes are recorded as having had a process, so both are in scope.
        assert_eq!(
            audit.resolved.iter().map(|r| r.pid).collect::<Vec<_>>(),
            vec![4242, 4243],
            "a finished node is audited too — skipping it is exactly how the leak goes unseen"
        );

        assert_eq!(
            audit
                .resolved
                .iter()
                .map(|r| r.standing.clone())
                .collect::<Vec<_>>(),
            vec![Standing::NotAttached, Standing::MarionSaysFinished],
            "and the two get different standings, which is what the claim turns on"
        );

        // The judgement itself, over the two standings once both are alive. Built directly rather
        // than through a tree, because what is under test is the rule and not the walk — and
        // because no journal can produce `AliveAndOurs` until `Spawned` carries an identity.
        let alive = |standing| Resolved {
            agent_id: AgentId("019f0000-0000-7000-8000-00000000000a".into()),
            pid: 4242,
            standing,
            resolution: Resolution::AliveAndOurs,
        };
        let orphan = alive(Standing::NotAttached);
        let finished = alive(Standing::MarionSaysFinished);
        assert!(
            !orphan.is_untracked_and_live(),
            "a running orphan is on the record and offered to the operator — §7.2 sanctions it, \
             and counting it would make the criterion unsatisfiable by its own scenario"
        );
        assert!(
            finished.is_untracked_and_live(),
            "a running process marion recorded as exited is the untracked runaway"
        );
        let mixed = Audit {
            resolved: vec![orphan, finished],
        };
        assert!(
            matches!(mixed.claim(), Claim::Fails(ref v) if v.len() == 1 && v[0].standing == Standing::MarionSaysFinished),
            "the claim names the leak and not the orphan: {:?}",
            mixed.claim()
        );
        assert_eq!(
            Audit {
                resolved: vec![alive(Standing::NotAttached)]
            }
            .claim(),
            Claim::Holds,
            "a fleet of running orphans and nothing else is a pass — marion accounts for all of them"
        );
    }

    /// An orphan with a pid and a node marion watched exit, also with a pid.
    fn tree_with_an_orphan_and_a_finished_node() -> Replay {
        let mut tree = tree_with_a_live_looking_node();
        let decided = AgentId("019f0000-0000-7000-8000-00000000000c".into());
        for (seq, kind) in [
            RecordKind::SpawnIntent(SpawnIntent {
                agent_id: decided.clone(),
                parent_id: None,
                agent_type: "claude".into(),
                harness: Harness::ClaudeCode,
                depth: 0,
                task_id: None,
            }),
            RecordKind::Spawned(Spawned {
                agent_id: decided.clone(),
                harness_version: "x".into(),
                model: None,
                pid: Some(4243),
                start_id: None,
            }),
            RecordKind::Exited(marion_core::journal::Exited {
                agent_id: decided.clone(),
                status: marion_core::contract::ExitStatus::Ok,
                exit: marion_core::contract::ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "child exited with code 0".into(),
                },
            }),
        ]
        .into_iter()
        .enumerate()
        {
            let rec = JournalRecord {
                writer: WriterId("w".into()),
                seq: 10 + seq as u64,
                ts: marion_core::encoding::SystemTime::from_unix_millis(
                    1_700_000_000_010 + seq as u64,
                ),
                mono_ns: 10 + seq as u64,
                provenance: Provenance::marion(),
                src_seq: None,
                kind,
            };
            tree.extend(format!("{}\n", serde_json::to_string(&rec).unwrap()).as_bytes());
        }
        tree
    }

    /// The same tree with **no** leak: both processes gone. This is what a pass looks like, and it
    /// is asserted so `Claim::Holds` is known to be reachable rather than merely declared.
    #[test]
    fn a_tree_whose_processes_are_all_gone_claims_no_untracked_live_process() {
        let tree = tree_with_an_orphan_and_a_finished_node();
        let audit = audit_with(&tree, |_| Read::NoSuchProcess);
        assert_eq!(audit.gone().len(), 2);
        assert_eq!(
            audit.claim(),
            Claim::Holds,
            "nothing wears either pid, so every process on the record is accounted for — and this \
             is definite with no recorded identity, which is why a dead pid is the one answer a \
             bare pid can still give"
        );
    }

    /// **The standing of a node does not change when the standard restart pass runs.**
    ///
    /// The regression this exists for was found by the end-to-end test, not by reasoning. `audit`
    /// first derived standing by re-running `restart::mark` and reading "no marking" as "marion
    /// decided" — true before `restart::apply`, and **false after it**, because `apply` moves a
    /// node to `Orphaned` and the unresolved clause then stops firing. Since §9's criterion is only
    /// ever evaluated *after* the restart pass, that made every healthy orphan report as a leak:
    /// the criterion would have failed loudly on a correct fleet, and the fix for that failure
    /// would very plausibly have been to stop counting leaks at all.
    #[test]
    fn the_standing_of_a_node_survives_the_restart_pass_that_marks_it_orphaned() {
        let mut tree = tree_with_an_orphan_and_a_finished_node();
        let before: Vec<Standing> = audit_with(&tree, |_| Read::NoSuchProcess)
            .resolved
            .iter()
            .map(|r| r.standing.clone())
            .collect();
        assert_eq!(
            before,
            vec![Standing::NotAttached, Standing::MarionSaysFinished],
            "the premise: one of each"
        );

        crate::restart::apply(&mut tree);
        let after: Vec<Standing> = audit_with(&tree, |_| Read::NoSuchProcess)
            .resolved
            .iter()
            .map(|r| r.standing.clone())
            .collect();
        assert_eq!(
            after, before,
            "`apply` records that marion did *not* decide these fates; an audit that read that as \
             a decision would call every orphan a leak, and would do so only in the order the \
             criterion is actually evaluated in"
        );
    }

    /// **A node that never reached a process has nothing to audit.**
    #[test]
    fn a_node_that_never_started_a_process_is_not_audited() {
        let mut tree = tree_with_a_live_looking_node();
        let rec = JournalRecord {
            writer: WriterId("w".into()),
            seq: 2,
            ts: marion_core::encoding::SystemTime::from_unix_millis(1_700_000_000_002),
            mono_ns: 2,
            provenance: Provenance::marion(),
            src_seq: None,
            kind: RecordKind::SpawnIntent(SpawnIntent {
                agent_id: AgentId("019f0000-0000-7000-8000-00000000000b".into()),
                parent_id: None,
                agent_type: "claude".into(),
                harness: Harness::ClaudeCode,
                depth: 0,
                task_id: None,
            }),
        };
        tree.extend(format!("{}\n", serde_json::to_string(&rec).unwrap()).as_bytes());
        let audit = audit_with(&tree, |_| Read::Id(sid("darwin-p_starttime:aa")));
        assert_eq!(
            audit.resolved.iter().map(|r| r.pid).collect::<Vec<_>>(),
            vec![4242],
            "the node that never reached `command.spawn()` has no process to account for: {:?}",
            audit.resolved
        );
    }

    // ---- the real syscall ------------------------------------------------------------------

    /// **The kernel read, against processes this test owns.**
    ///
    /// Real rather than mocked, because the value of this function is entirely in whether it agrees
    /// with the kernel — a mock would be a test of the test. Three facts, each measured before it
    /// was relied on: a live process answers, the answer is stable, and two different processes do
    /// not share an identity.
    #[test]
    fn the_kernel_read_identifies_a_live_process_and_is_stable() {
        let me = std::process::id() as i32;
        let Read::Id(first) = read(me) else {
            panic!(
                "this process is running, so the kernel must describe it: {:?}",
                read(me)
            )
        };
        assert_eq!(
            read(me),
            Read::Id(first.clone()),
            "a start identity does not change while the process runs — if it did, every \
             comparison this module makes would be worthless"
        );
        assert!(
            first.0.starts_with("darwin-p_starttime:"),
            "tagged, so a journal carried to another platform cannot produce a false match: {first}"
        );

        let child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("a child to identify");
        let child_pid = child.id() as i32;
        let Read::Id(theirs) = read(child_pid) else {
            panic!("the child is running")
        };
        assert_ne!(
            theirs, first,
            "two processes started at different instants must not share an identity, or a \
             recycled pid would read as a survivor"
        );
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
    }

    /// **A pid with no process is `NoSuchProcess`, and it is reached through success rather than
    /// through an error.**
    ///
    /// Measured: `sysctl` answers a dead pid with return code 0 and zero bytes. A reader that
    /// treated only a non-zero return as meaningful would report `Unavailable` here and lose the
    /// one definite answer available with no recorded identity — which would make §9's criterion
    /// unprovable for *every* node rather than only for live ones.
    #[test]
    fn a_reaped_pid_reads_as_no_such_process() {
        let mut child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("a child to reap");
        let pid = child.id() as i32;
        child.wait().expect("reaped, so the pid stops resolving");
        assert_eq!(
            read(pid),
            Read::NoSuchProcess,
            "the process is gone and reaped, so nothing wears the pid"
        );
    }

    /// A pid that is not a process id at all is refused rather than asked about.
    ///
    /// `0` and negatives name process *groups* to `kill(2)`, so passing them to a per-process query
    /// asks a different question than the caller thinks they asked.
    #[test]
    fn a_non_positive_pid_is_refused_rather_than_asked_about() {
        for pid in [0, -1, -4242] {
            assert!(
                matches!(read(pid), Read::Unavailable(_)),
                "{pid} is not a process id"
            );
        }
    }
}
