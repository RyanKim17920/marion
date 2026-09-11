//! The registry journal's **records and their framing** (design §4.3).
//!
//! > *"**The registry is an append-only journal**, replayed at startup, not a rewritten
//! > `registry.json`: rewriting the whole tree per state change is O(tree) per event against an
//! > intentionally unbounded tree. **Spawn is journaled as intent-then-confirm** — a crash between
//! > 'process started' and 'registry updated' would otherwise leave a live child with no registry
//! > entry, holding a session id the ownership invariant no longer knows about."*
//!
//! This module is pure data plus the line codec. The file lives in the supervisor
//! (`marion_supervisor::journal`), because `marion-core` performs no I/O; replay lives in
//! the registry module one layer up, because §8 lists *"journal replay"* as an **L1 pure unit**.
//!
//! # Framing
//!
//! One record per line, compact JSON, `\n`-terminated — the file is `journal.jsonl` (§4.3) and
//! §7.4 says *"a truncated final line is discarded on replay"*, so the delimiter **is** the frame.
//! Two properties make that safe rather than merely conventional:
//!
//! * `serde_json`'s compact form emits no literal newline (a newline inside a string is escaped as
//!   `\n`), so **any prefix of a record is a prefix of exactly one line**. A torn tail cannot
//!   forge a frame boundary.
//! * every record is capped at [`MAX_RECORD_BYTES`] and written with **one** `write(2)` under
//!   `O_APPEND`, which is what makes two writer processes interleave at record granularity rather
//!   than byte granularity. The cap is enforced here, at encode time, so the invariant the
//!   concurrency argument rests on cannot be violated by a caller.
//!
//! # Durability
//!
//! §4.3: *"Append without fsync; fsync on a ~50 ms timer **and** unconditionally at each state
//! transition that must survive a crash — `Spawned`, `Exited`, `ReapedIdle`."* [`RecordKind::is_barrier`]
//! is that list. Per-record fsync was **replaced** by group commit (§12) and is not reintroduced.

use serde::{Deserialize, Serialize};

use crate::contract::{AgentId, ExitStatus, ProcessExit, ResultStatus, TaskId};
use crate::harness::Harness;
use crate::ir::{Provenance, SrcSeq};
use crate::node::NodeState;

/// The hard cap on one encoded record, **newline included**.
///
/// It is the size limit the `O_APPEND` concurrency argument holds under: POSIX makes the
/// offset-and-write of an `O_APPEND` write atomic with respect to other writers, and a single
/// `write(2)` of this size to a local regular file is not split by any filesystem marion runs on.
/// 16 KiB is far above what any record here can reach — the largest field is a `ProcessExit`
/// description marion writes itself — and far below the point where a short write becomes a
/// practical concern. A record that would exceed it is a **refusal**, not a silent truncation:
/// truncating would produce a line that parses as a different, wrong record.
pub const MAX_RECORD_BYTES: usize = 16 * 1024;

/// Which process wrote a record.
///
/// **There is no global sequence number, and that is a decision.** `marion run` and each
/// `marion-supervisor mcp` bridge are separate processes that both cause lifecycle events, and a
/// monotonic counter shared across them would need exactly the coordination the append-only design
/// exists to avoid. So the journal's total order is the **file's byte order** — the order the
/// kernel serialized the appends in — and `seq` is per-writer, which is what makes loss detectable
/// at all: a gap in one writer's ordinals is a lost record, in precisely §4.2's `Ordinal` sense.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WriterId(pub String);

/// One line of `journal.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRecord {
    pub writer: WriterId,
    /// This writer's ordinal, from 0, **gapless by construction**. See [`WriterId`].
    pub seq: u64,
    /// §4/§6.7 encoding: RFC3339 UTC, literal `Z`, three fractional digits. Display only — it is
    /// wall clock, and §4.2 says NTP steps and sleep/wake move it backwards.
    pub ts: crate::encoding::SystemTime,
    /// Monotonic since this writer started. Not comparable across writers, and not the order
    /// replay uses; it is here for the same reason §4.2 gives — aligning a record with `pty.cast`.
    pub mono_ns: u64,
    pub provenance: Provenance,
    /// §4.2. `None` on every record marion writes about its own decisions, because marion *is* the
    /// source and the source has no upstream ordering to report. Present when a record is caused by
    /// a harness event that carried evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src_seq: Option<SrcSeq>,
    pub kind: RecordKind,
}

impl JournalRecord {
    /// §4.3's barrier list, forwarded so callers need not reach through `kind`.
    pub fn is_barrier(&self) -> bool {
        self.kind.is_barrier()
    }

    /// The `AgentId` this record is about, or `None` for the supervisor's own exit.
    ///
    /// §5.7 makes that one absence load-bearing: assigning an arbitrary node to a process-wide
    /// exit would make the journal claim a relationship that did not occur. The replay remains a
    /// single pass because the exceptional record changes no node at all.
    pub fn agent_id(&self) -> Option<&AgentId> {
        self.kind.agent_id()
    }
}

/// What a record says. Externally tagged (`{"Spawned":{…}}`), matching §6.7's `Workspace` — one
/// tagging convention across marion's persisted JSON, not two.
///
/// **Additive by rule.** A new variant may be added; an existing one may only gain
/// `#[serde(default)]` fields. Both directions are exercised in this module's tests, because "an
/// older record must still deserialize" is the property the journal's whole value rests on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordKind {
    /// §6.1 step 7, first half: *"Journal the spawn intent, start the process, journal
    /// confirmation."* Written **before** any process exists, so a crash in the window leaves an
    /// intent with no confirmation — recoverable, rather than a live child with no registry entry.
    SpawnIntent(SpawnIntent),
    /// §6.1 step 7, second half. The process exists.
    Spawned(Spawned),
    /// The intent's other resolution: marion started the process and then abandoned the spawn
    /// (§6.1 step 8's 30 s bridge cap kills it and *"journal[s] the abort against the intent
    /// record"*). Recorded so the node is never mistaken for one marion *lost* — §7.2 is emphatic
    /// that a node marion decided the fate of is never `Orphaned`.
    SpawnAborted(SpawnAborted),
    /// §3.2's `state`, moved.
    StateChanged(StateChanged),
    /// The node's terminal transition. Carries the terminal status and the `ProcessExit` §6.7
    /// records, so replay reconstructs the outcome without reading the contract file.
    Exited(Exited),
    /// §7.2: *"Journaled **before** the kill, as an intent record."* A single record written
    /// before the kill would leave a crash window in which restart reads `ReapedIdle`, skips the
    /// `Orphaned` marking, and a live process survives untracked and unkillable.
    ReapIntent(ReapIntent),
    /// The process was observed dead. §7.2: an unconfirmed intent resolves to `ReapedIdle` either
    /// way — that resolution is restart policy and is **not** replay's job.
    ReapConfirmed(ReapConfirmed),
    /// §6.7/§7.3.2(a), durable before the first signal. `was` is the state in the rendered list the
    /// operator confirmed; omitting it would leave the audit record unable to say what was lost.
    KillIntent(KillIntent),
    /// The process was observed dead after marion's per-node two-step group kill. This is the
    /// terminal transition to `Cancelled`; an additional `Exited` would assert the same fact twice.
    KillConfirmed(KillConfirmed),
    /// §4.3's `contracts/<task_id>.json` was written. The journal records *that a contract exists
    /// and how it ended*, never its contents: the file is the contract, and copying it here would
    /// be a second source of truth for a document §6.7 already makes authoritative.
    ContractPersisted(ContractPersisted),
    /// §7.1/§9: a permission marion refused. Recorded **in the journal, not in a contract** —
    /// §9 says so in as many words, because the node it happens to may be a root, and a root has
    /// no contract to record it in.
    PermissionDenied(PermissionDenied),
    /// §9: what a **root** did to the operator's own repository, and whether marion looked at all.
    ///
    /// The same shape as [`Self::ContractPersisted`], for the same reason: this record is O(1) in
    /// what the root did, and `<agent-dir>/root-change.json` is authoritative for the paths and the
    /// patch. It carries no path to that file — the leaf is a constant derivable from the
    /// `AgentId` ([`crate::paths::AgentDir::root_change`]) — exactly as `ContractPersisted` carries
    /// none. See [`crate::root_change`] for why a path list cannot live in a journal record.
    RootChanged(crate::root_change::RootChanged),
    /// §9: a root's availability axis was decided, and this is the tree it was decided against.
    ///
    /// **The intent half of [`Self::RootChanged`]**, and it exists for §6.1 step 7's reason rather
    /// than for symmetry: `RootChanged` is written when the run *returns*, so a marion that dies
    /// mid-run left no evidence that a grant had been issued at all. Same discipline as
    /// [`Self::SpawnIntent`] — write the intent, do the act, confirm — applied to the other thing
    /// `root::prepare` does.
    RootGrantDecided(crate::root_change::RootGrant),
    /// The harness's own name for this node's conversation, as marion observed it.
    ///
    /// **A separate record, not a field on [`Self::Spawned`]**, because the two are known at
    /// different instants: `Spawned` is written the moment the process exists, and the session id
    /// arrives in the harness's first frame — Claude Code's `system/init.session_id`, Codex's
    /// `thread.started.thread_id` — some time after. Folding it onto `Spawned` would mean either
    /// delaying the barrier record past the window it exists to cover, or writing a `Spawned` and
    /// then rewriting it, which an append-only journal cannot do. It carries the `harness` beside
    /// the id because the id is meaningful only in that harness's own resume grammar, and a reader
    /// handing it back must not have to look up the intent to know whose grammar that is.
    ///
    /// **Not a barrier.** Losing one on the ~50 ms timer costs the ability to resume this node; it
    /// does not leave a process untracked, and §4.3 pays for a barrier only for the latter.
    SessionObserved(SessionObserved),
    /// §5.7's ordinary exit record. It deliberately carries no node id: the supervisor serves a
    /// forest, and choosing one node would fabricate ownership of a process-wide event.
    SupervisorExited(SupervisorExited),
}

impl RecordKind {
    /// §4.3's barrier set: *"`Spawned`, `Exited`, `ReapedIdle`"*, plus the **intent** records those
    /// three are the confirmations of — §4.3 extends the barrier to the intent explicitly ("fsync
    /// the intent, do the act, then append and fsync the confirmation"), and an intent that is not
    /// durable before the act buys nothing at all.
    ///
    /// Everything else rides the ~50 ms group-commit timer. The cost of losing one of those is a
    /// slightly stale replay; the cost of losing a barrier record is an untracked live process,
    /// and §4.3 says only the latter pays for a barrier.
    pub fn is_barrier(&self) -> bool {
        matches!(
            self,
            RecordKind::SpawnIntent(_)
                | RecordKind::Spawned(_)
                | RecordKind::SpawnAborted(_)
                | RecordKind::Exited(_)
                | RecordKind::ReapIntent(_)
                | RecordKind::ReapConfirmed(_)
                | RecordKind::KillIntent(_)
                | RecordKind::KillConfirmed(_)
                | RecordKind::SupervisorExited(_)
                // A record whose *whole purpose* is to survive a crash, and which is therefore
                // worth nothing on the ~50 ms group-commit timer: the window it exists to cover
                // opens the instant `prepare` returns. §4.3's rule — fsync the intent, then do the
                // act — is the same argument that puts `SpawnIntent` in this set, and the act here
                // is handing a root a tool on the operator's own checkout.
                | RecordKind::RootGrantDecided(_)
        )
    }

    pub fn agent_id(&self) -> Option<&AgentId> {
        match self {
            RecordKind::SpawnIntent(r) => Some(&r.agent_id),
            RecordKind::Spawned(r) => Some(&r.agent_id),
            RecordKind::SpawnAborted(r) => Some(&r.agent_id),
            RecordKind::StateChanged(r) => Some(&r.agent_id),
            RecordKind::Exited(r) => Some(&r.agent_id),
            RecordKind::ReapIntent(r) => Some(&r.agent_id),
            RecordKind::ReapConfirmed(r) => Some(&r.agent_id),
            RecordKind::KillIntent(r) => Some(&r.agent_id),
            RecordKind::KillConfirmed(r) => Some(&r.agent_id),
            RecordKind::ContractPersisted(r) => Some(&r.agent_id),
            RecordKind::PermissionDenied(r) => Some(&r.agent_id),
            RecordKind::RootChanged(r) => Some(&r.agent_id),
            RecordKind::RootGrantDecided(r) => Some(&r.agent_id),
            RecordKind::SessionObserved(r) => Some(&r.agent_id),
            RecordKind::SupervisorExited(_) => None,
        }
    }
}

/// Everything about a node that never changes, written once, before the process exists.
///
/// This is §4's `Lifecycle::Spawned` payload **minus what `marion-core` cannot name**: `isolation`,
/// `caps` and `surfaces` are `marion-harness` types, and core does not depend on harness (the
/// dependency runs the other way). §4.3 makes `meta.json` the home of *"compiled spec, caps,
/// harness ref"*, so carrying them here too would be a second source of truth for the same three
/// facts — but **`meta.json` is declared and unwritten** ([`crate::paths`]), so today those three
/// are recoverable from *nothing*. That is a gap in what marion records, and naming it is worth more
/// than the layering argument: adding them here would put harness types in core and still be the
/// wrong home. `harness_version` and `model` are on [`Spawned`] instead of here, because both are
/// resolved by *launching* and are not known at intent time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnIntent {
    pub agent_id: AgentId,
    /// §7.5: **immutable**. The tree never silently re-parents, so this is written once and
    /// replay never updates it.
    pub parent_id: Option<AgentId>,
    /// The **canonical** agent-type name, not the alias the caller used.
    pub agent_type: String,
    pub harness: Harness,
    /// §3.1's depth, root = 0.
    pub depth: u32,
    /// The contract this node's run is under. `None` for a **root**, which §9 says has no
    /// contract — not a placeholder, an absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// **§3.1's node-level bound, as it was resolved for *this* launch** — `marion run --timeout`
    /// or `spawn`'s `timeout_secs`, already clamped and defaulted, never the number the caller
    /// typed. It belongs on the intent for the same reason [`Self::depth`] does: it is decided
    /// before the process exists and never changes afterwards.
    ///
    /// **Recorded because it cannot be re-derived.** The agent type carries a bound too, but it is
    /// the type's *default* and not the node's: a reader that resolves the bound from the type
    /// reports 900 s for a root launched with `--timeout 300` and for a child spawned with
    /// `timeout_secs: 120`, which is what `marion tree`'s detail pane did. Two sources of one fact
    /// would be the worry if the type were still consulted for a node that has one here; it is not
    /// — this outranks it, and the type answers only where this is `None`.
    ///
    /// **Additive**, per this enum's rule: `#[serde(default)]` so every journal written before it
    /// still replays, and `skip_serializing_if` so an absent bound is byte-identical to what the
    /// previous build wrote. `None` means *the journal does not say* — an older record, or a
    /// launch marion placed no bound of its own on — and a reader falls back to the agent type
    /// rather than inventing a number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// §6.1 step 7's confirmation: the process exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spawned {
    pub agent_id: AgentId,
    /// Resolved at launch (§3.2), so it cannot be on the intent.
    pub harness_version: String,
    /// The model that actually reached the harness, in the harness's own spelling — the same
    /// measurement `TaskContract.child.model` records, and `None` for the same reason (`codex
    /// exec` takes no model argument at all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `None` where marion drove the process through a helper that does not surface one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
    /// **What makes [`Self::pid`] identify a process rather than merely address one.**
    ///
    /// A pid is a signal target; it is not an identity. After a crash of unknown duration the
    /// kernel may have handed the number to something else, so §7.2's probe branch — *"resolved by
    /// checking for the process"* — could not be answered from `pid` alone, and `restart.rs`
    /// refuses to guess. With this beside it the answer becomes definite in both directions: the
    /// same identity means the node survived, a different one means the pid was recycled and the
    /// node is gone. See [`crate::node::StartId`] for why it is opaque and equality-only, and
    /// `marion-supervisor`'s `procid` for what reads it.
    ///
    /// **Additive**, per this enum's own rule: `#[serde(default)]` so every journal written before
    /// it still replays, and `skip_serializing_if` so a record without one is byte-identical to
    /// what the previous build wrote. `None` therefore means two things that need no distinguishing
    /// — an older journal, or a platform where marion cannot read one — and both resolve to
    /// *cannot-tell*, which is correct for each.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_id: Option<crate::node::StartId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnAborted {
    pub agent_id: AgentId,
    /// marion's own explanation, never derived from an exit code — the same discipline
    /// `ProcessExit.description` holds to.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateChanged {
    pub agent_id: AgentId,
    pub state: NodeState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exited {
    pub agent_id: AgentId,
    pub status: ExitStatus,
    pub exit: ProcessExit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReapIntent {
    pub agent_id: AgentId,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReapConfirmed {
    pub agent_id: AgentId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillIntent {
    pub agent_id: AgentId,
    pub was: NodeState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillConfirmed {
    pub agent_id: AgentId,
    pub exit: ProcessExit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorExited {}

/// See [`RecordKind::SessionObserved`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionObserved {
    pub agent_id: AgentId,
    /// Whose grammar `session_id` belongs to. Written rather than looked up from the intent so the
    /// record is self-describing to a reader that has only this line.
    pub harness: Harness,
    /// Opaque to marion: the harness's spelling, kept verbatim, handed back verbatim.
    pub session_id: String,
    /// **The shape this node was launched in**, recorded here so a resume reconstructs the launch
    /// from a written value rather than inferring it (`plan-restart-resume.md` step 6): `true` is a
    /// pane (a pty marion owns), `false` the headless launch. It rides this record because this is
    /// the record a resume reads — a session and the shape that produced it, together — and because
    /// only a launch that emitted a frame has one at all. `false` today on every resumable node: a
    /// pane emits no `stream-json`, so it names no session, so a pane node is never resumed anyway;
    /// the field is written so that stays a checked refusal and never a silent headless relaunch.
    ///
    /// **Additive**, this enum's rule: `#[serde(default)]` so older journals replay, and
    /// `skip_serializing_if` so a headless node's record is byte-identical to what earlier builds
    /// wrote.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub pane: bool,
    /// **Where this node ran, and under which of §6.6's two workspaces** — the other half of the
    /// launch a relaunch has to reconstruct, and the half a *child* cannot do without.
    ///
    /// A root's cwd is derivable: it is the working tree of the project this supervisor is keyed
    /// on. A child's is not. Under [`Isolation::Worktree`](crate::contract::Isolation::Worktree) it
    /// is a linked worktree marion created under `.marion/worktrees/…` on a `marion/<task-id>`
    /// branch; under `SharedCwd` it is the caller's own directory. Nothing else on the journal says
    /// which of the two it was or where it is, and a harness resumes a session only from the cwd it
    /// was created in — so without this record a child's resume could only be refused by name.
    ///
    /// [`Workspace`](crate::contract::Workspace) rather than a bare path because the *kind* is
    /// load-bearing on the way back in: a worktree relaunch must reuse the existing tree and its
    /// branch (creating a second one would be a second tree the resumed session has never seen),
    /// and a `SharedCwd` relaunch must retake §6.6's occupancy claim. One value carries the cwd and
    /// the isolation kind together, so the two can never be recorded and read apart.
    ///
    /// **Additive**, this enum's rule: `#[serde(default)]` so older journals replay, and
    /// `skip_serializing_if` so a record without one is byte-identical to what earlier builds
    /// wrote. `None` means *the journal does not say*, and a reader that needs one refuses by name
    /// rather than relaunching in whatever directory is at hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<crate::contract::Workspace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractPersisted {
    /// The node the contract is *about* — the child. `requester` is the node that asked.
    pub agent_id: AgentId,
    pub task_id: TaskId,
    pub requester: AgentId,
    /// `Some` iff the contract carries a `Completion` (§6.7): `None` while the run is live, and
    /// `None` if it ended unobserved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ResultStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionDenied {
    pub agent_id: AgentId,
    pub tool: String,
    pub reason: String,
}

/// Encoding refused a record. Both variants are refusals rather than repairs, for the same reason:
/// a shortened record is a *different* record, and one that parses.
#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error(
        "a journal record of {0} bytes exceeds the {MAX_RECORD_BYTES}-byte cap that makes an \
         O_APPEND write atomic against a concurrent writer; refused rather than truncated, since \
         a truncated record would deserialize as a different one"
    )]
    TooLarge(usize),
    #[error("serializing a journal record: {0}")]
    Json(#[from] serde_json::Error),
}

/// One record as its line, newline included. **The only place a record becomes bytes.**
pub fn encode(record: &JournalRecord) -> Result<Vec<u8>, EncodeError> {
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    if line.len() > MAX_RECORD_BYTES {
        return Err(EncodeError::TooLarge(line.len()));
    }
    debug_assert!(
        line[..line.len() - 1].iter().all(|b| *b != b'\n'),
        "compact JSON escapes newlines; a raw one would forge a frame boundary"
    );
    Ok(line)
}

/// One line back to a record. `None` for anything that is not a complete, valid record — replay
/// treats that as the end of the intact prefix.
pub fn decode(line: &[u8]) -> Option<JournalRecord> {
    serde_json::from_slice(line).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::SystemTime;

    fn rec(kind: RecordKind) -> JournalRecord {
        JournalRecord {
            writer: WriterId("w-1".into()),
            seq: 3,
            ts: SystemTime::from_unix_millis(1_785_625_628_619),
            mono_ns: 42,
            provenance: Provenance::marion(),
            src_seq: None,
            kind,
        }
    }

    fn intent() -> RecordKind {
        RecordKind::SpawnIntent(SpawnIntent {
            agent_id: AgentId("a-1".into()),
            parent_id: Some(AgentId("root".into())),
            agent_type: "codex-impl".into(),
            harness: Harness::Codex,
            depth: 1,
            task_id: Some(TaskId("t-1".into())),
            timeout_secs: None,
        })
    }

    #[test]
    fn a_record_pins_its_wire_shape() {
        // The whole envelope, byte for byte. Every field here is read by a replayer that may be a
        // different build of marion than the one that wrote it.
        assert_eq!(
            serde_json::to_value(rec(intent())).unwrap(),
            serde_json::json!({
                "writer": "w-1",
                "seq": 3,
                "ts": "2026-08-01T23:07:08.619Z",
                "mono_ns": 42,
                "provenance": {
                    "source": "Marion",
                    "source_id": null,
                    "observed_live": true,
                    "authoritative": true,
                    "completeness": "Complete",
                    "transformation": "Native",
                },
                "kind": {"SpawnIntent": {
                    "agent_id": "a-1",
                    "parent_id": "root",
                    "agent_type": "codex-impl",
                    "harness": "codex",
                    "depth": 1,
                    "task_id": "t-1",
                }},
            }),
            "the harness must still be the bare wire string `codex`, as `ChildRef` writes it"
        );
    }

    #[test]
    fn an_absent_src_seq_is_absent_from_the_wire_not_null() {
        let json = serde_json::to_string(&rec(intent())).unwrap();
        assert!(
            !json.contains("src_seq"),
            "§4.2: where there is no ordering evidence the record must not imply any; got {json}"
        );
        let with = JournalRecord {
            src_seq: Some(SrcSeq::Predecessor(crate::ir::EventId("u-9".into()))),
            ..rec(intent())
        };
        assert!(
            serde_json::to_string(&with)
                .unwrap()
                .contains(r#""src_seq":{"Predecessor":"u-9"}"#)
        );
    }

    #[test]
    fn every_kind_round_trips_through_a_line() {
        let exit = ProcessExit {
            code: Some(0),
            signal: None,
            description: "clean exit".into(),
        };
        let kinds = [
            intent(),
            RecordKind::Spawned(Spawned {
                agent_id: AgentId("a-1".into()),
                harness_version: "2.1.220".into(),
                model: Some("gpt-5.4".into()),
                pid: Some(4242),
                start_id: None,
            }),
            RecordKind::SpawnAborted(SpawnAborted {
                agent_id: AgentId("a-1".into()),
                reason: "the bridge never handshook within 30 s".into(),
            }),
            RecordKind::StateChanged(StateChanged {
                agent_id: AgentId("a-1".into()),
                state: NodeState::Blocked(crate::node::BlockReason::Descendants),
            }),
            RecordKind::Exited(Exited {
                agent_id: AgentId("a-1".into()),
                status: ExitStatus::Ok,
                exit: exit.clone(),
            }),
            RecordKind::ReapIntent(ReapIntent {
                agent_id: AgentId("a-1".into()),
                reason: "idle memory reclaim".into(),
            }),
            RecordKind::ReapConfirmed(ReapConfirmed {
                agent_id: AgentId("a-1".into()),
            }),
            RecordKind::KillIntent(KillIntent {
                agent_id: AgentId("a-1".into()),
                was: NodeState::Running,
            }),
            RecordKind::KillConfirmed(KillConfirmed {
                agent_id: AgentId("a-1".into()),
                exit: ProcessExit {
                    code: None,
                    signal: Some(9),
                    description: "marion sent SIGKILL".into(),
                },
            }),
            RecordKind::ContractPersisted(ContractPersisted {
                agent_id: AgentId("a-1".into()),
                task_id: TaskId("t-1".into()),
                requester: AgentId("root".into()),
                status: Some(ExitStatus::Unreported),
            }),
            RecordKind::PermissionDenied(PermissionDenied {
                agent_id: AgentId("root".into()),
                tool: "Bash".into(),
                reason: "the root's Blocked bound expired unanswered".into(),
            }),
            RecordKind::SessionObserved(SessionObserved {
                agent_id: AgentId("a-1".into()),
                harness: Harness::Codex,
                session_id: "thr_01".into(),
                pane: false,
                workspace: None,
            }),
            RecordKind::SupervisorExited(SupervisorExited {}),
        ];
        for kind in kinds {
            let r = rec(kind);
            let line = encode(&r).unwrap();
            assert_eq!(line.last(), Some(&b'\n'));
            assert_eq!(decode(&line[..line.len() - 1]).as_ref(), Some(&r));
        }
    }

    #[test]
    fn the_barrier_set_is_section_4_3s_list_and_its_intents() {
        let a = AgentId("a".into());
        assert!(
            RecordKind::SpawnIntent(SpawnIntent {
                agent_id: a.clone(),
                parent_id: None,
                agent_type: "claude".into(),
                harness: Harness::ClaudeCode,
                depth: 0,
                task_id: None,
                timeout_secs: None,
            })
            .is_barrier()
        );
        assert!(
            RecordKind::ReapIntent(ReapIntent {
                agent_id: a.clone(),
                reason: String::new()
            })
            .is_barrier()
        );
        assert!(
            RecordKind::ReapConfirmed(ReapConfirmed {
                agent_id: a.clone()
            })
            .is_barrier()
        );
        assert!(
            RecordKind::KillIntent(KillIntent {
                agent_id: a.clone(),
                was: NodeState::Running,
            })
            .is_barrier()
        );
        assert!(
            RecordKind::KillConfirmed(KillConfirmed {
                agent_id: a.clone(),
                exit: ProcessExit {
                    code: None,
                    signal: Some(9),
                    description: "confirmed".into(),
                },
            })
            .is_barrier()
        );
        assert!(RecordKind::SupervisorExited(SupervisorExited {}).is_barrier());
        // Not barriers: losing one costs a stale replay, not an untracked process (§4.3).
        assert!(
            !RecordKind::StateChanged(StateChanged {
                agent_id: a.clone(),
                state: NodeState::Running,
            })
            .is_barrier()
        );
        assert!(
            !RecordKind::ContractPersisted(ContractPersisted {
                agent_id: a.clone(),
                task_id: TaskId("t".into()),
                requester: a,
                status: None,
            })
            .is_barrier()
        );
    }

    #[test]
    fn an_older_record_still_deserializes() {
        // Written by a build that had no `src_seq`, no `model`, no `pid` and no `status` — every
        // field this module marks `#[serde(default)]`. The rule is additive-only, and this is the
        // test that would fail the day someone makes a field required.
        let old = br#"{"writer":"w-0","seq":0,"ts":"2026-08-01T23:07:08.619Z","mono_ns":1,
            "provenance":{"source":"Marion"},
            "kind":{"Spawned":{"agent_id":"a-1","harness_version":"2.1.220"}}}"#;
        let compact: Vec<u8> = old.iter().copied().filter(|b| *b != b'\n').collect();
        let r = decode(&compact).expect("an older record must still read");
        assert_eq!(r.src_seq, None);
        match r.kind {
            RecordKind::Spawned(s) => {
                assert_eq!(s.harness_version, "2.1.220");
                assert_eq!(s.model, None);
                assert_eq!(s.pid, None);
                assert_eq!(s.start_id, None);
            }
            other => panic!("{other:?}"),
        }
    }

    /// **`start_id` is additive in both directions, which is what lets it be added at all.**
    ///
    /// This enum's rule is that an existing variant may only gain `#[serde(default)]` fields, and
    /// the reason is stated as *"an older record must still deserialize"* — the property the
    /// journal's whole value rests on. The half that rule does not say out loud, and that matters
    /// just as much here, is the **forward** direction: a build that has the field but nothing to
    /// put in it must keep writing exactly what the previous build wrote. Otherwise every existing
    /// journal gains a `"start_id":null` at the first append, every byte-comparison fixture moves,
    /// and the change stops being additive in practice however additive it is in principle.
    #[test]
    fn a_spawned_without_a_start_id_is_byte_identical_to_what_the_previous_build_wrote() {
        let without = Spawned {
            agent_id: AgentId("a-1".into()),
            harness_version: "2.1.220".into(),
            model: None,
            pid: Some(4242),
            start_id: None,
        };
        let line = serde_json::to_string(&RecordKind::Spawned(without)).unwrap();
        assert_eq!(
            line, r#"{"Spawned":{"agent_id":"a-1","harness_version":"2.1.220","pid":4242}}"#,
            "`skip_serializing_if` is what keeps this true — without it the field would appear as \
             `null` on every record marion has ever written"
        );

        let with = Spawned {
            agent_id: AgentId("a-1".into()),
            harness_version: "2.1.220".into(),
            model: None,
            pid: Some(4242),
            start_id: Some(crate::node::StartId("darwin-p_starttime:ab".into())),
        };
        let line = serde_json::to_string(&RecordKind::Spawned(with.clone())).unwrap();
        assert!(
            line.contains(r#""start_id":"darwin-p_starttime:ab""#),
            "and when there is one it is a plain string, not a wrapper object: {line}"
        );
        assert_eq!(
            serde_json::from_str::<RecordKind>(&line).unwrap(),
            RecordKind::Spawned(with),
            "round-trips, so a replay reads back the identity a spawn recorded"
        );
    }

    /// **The bound is additive in both directions, for [`Spawned::start_id`]'s reasons.**
    ///
    /// An intent written before the field existed must still replay — it does, as `None`, which a
    /// reader resolves through the agent type exactly as it did before the field was added — and a
    /// build that has the field but no bound to record must write the byte-identical line the
    /// previous build wrote.
    #[test]
    fn a_spawn_intent_carries_its_bound_and_omits_it_when_there_is_none() {
        let without = SpawnIntent {
            agent_id: AgentId("a-1".into()),
            parent_id: None,
            agent_type: "claude".into(),
            harness: Harness::ClaudeCode,
            depth: 0,
            task_id: None,
            timeout_secs: None,
        };
        assert_eq!(
            serde_json::to_string(&RecordKind::SpawnIntent(without)).unwrap(),
            r#"{"SpawnIntent":{"agent_id":"a-1","parent_id":null,"agent_type":"claude","harness":"claude-code","depth":0}}"#,
            "`skip_serializing_if` is what keeps an unbounded intent byte-identical to what every \
             build before this field wrote"
        );

        let with = SpawnIntent {
            agent_id: AgentId("a-1".into()),
            parent_id: None,
            agent_type: "claude".into(),
            harness: Harness::ClaudeCode,
            depth: 0,
            task_id: None,
            timeout_secs: Some(300),
        };
        let line = serde_json::to_string(&RecordKind::SpawnIntent(with.clone())).unwrap();
        assert!(
            line.contains(r#""timeout_secs":300"#),
            "an operator's `--timeout 300` is on the record as a plain number: {line}"
        );
        assert_eq!(
            serde_json::from_str::<RecordKind>(&line).unwrap(),
            RecordKind::SpawnIntent(with),
            "round-trips, so replay reads back the clock the launch resolved"
        );

        // An intent from a build that predates the field.
        let old = br#"{"SpawnIntent":{"agent_id":"a-1","parent_id":null,"agent_type":"claude",
            "harness":"claude-code","depth":0}}"#;
        let compact: Vec<u8> = old.iter().copied().filter(|b| *b != b'\n').collect();
        match serde_json::from_slice::<RecordKind>(&compact).expect("an older intent must read") {
            RecordKind::SpawnIntent(i) => assert_eq!(
                i.timeout_secs, None,
                "the journal does not say, which is not the same as 900"
            ),
            other => panic!("{other:?}"),
        }
    }

    /// **NC — the supervisor exit is not assigned to a convenient node.** A fabricated id would
    /// make a process-wide event look like part of one task's history, and on an empty forest there
    /// is not even a candidate to fabricate.
    #[test]
    fn the_supervisor_exit_record_has_no_agent_id() {
        let r = rec(RecordKind::SupervisorExited(SupervisorExited {}));
        assert_eq!(r.agent_id(), None);
        assert_eq!(r.kind.agent_id(), None);
    }

    #[test]
    fn a_record_that_would_break_append_atomicity_is_refused_not_truncated() {
        let huge = RecordKind::SpawnAborted(SpawnAborted {
            agent_id: AgentId("a-1".into()),
            reason: "x".repeat(MAX_RECORD_BYTES),
        });
        let e = encode(&rec(huge)).unwrap_err();
        assert!(
            matches!(e, EncodeError::TooLarge(n) if n > MAX_RECORD_BYTES),
            "{e}"
        );
    }

    #[test]
    fn a_newline_in_a_field_is_escaped_and_never_forges_a_frame() {
        // The whole framing argument rests on this: a record's encoding contains exactly one raw
        // newline, its terminator. A description carrying a literal newline must not split it.
        let r = rec(RecordKind::Exited(Exited {
            agent_id: AgentId("a-1".into()),
            status: ExitStatus::Killed,
            exit: ProcessExit {
                code: None,
                signal: Some(9),
                description: "line one\nline two\r\n".into(),
            },
        }));
        let line = encode(&r).unwrap();
        assert_eq!(
            line.iter().filter(|b| **b == b'\n').count(),
            1,
            "one raw newline, the terminator"
        );
        assert_eq!(decode(&line[..line.len() - 1]).as_ref(), Some(&r));
    }

    #[test]
    fn decode_rejects_rather_than_guesses() {
        crate::encoding::assert_decode_rejects_rather_than_guesses(decode, b"{\"writer\":\"w\"");
    }
}
