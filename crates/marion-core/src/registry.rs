//! **Journal replay** — the registry, reconstructed from `journal.jsonl` (design §4.3, §7.4).
//!
//! §8 lists *"journal replay"* among the **L1 pure units**, and this is why it lives in
//! `marion-core` rather than beside the writer: replay takes bytes and returns a tree, touches no
//! filesystem and consults no clock, so it is testable against a byte string with no temp dir.
//!
//! # What replay is measured against
//!
//! §9's M2 criterion, in full: the replayed tree is *"**structurally identical** … same nodes,
//! same parent edges, same terminal states, same contracts. **Compared against the journal as of
//! the replay's own read, not against a live tree that kept moving**"*. That sentence is the
//! specification of this module: it reconstructs exactly what the journal records, and it neither
//! infers nor probes anything the journal does not say.
//!
//! # What replay deliberately does not do
//!
//! * **It never produces `Orphaned`.** §7.2 marks `Live` → `Orphaned` *on restart*, and only after
//!   deciding that marion has no record of the node's fate. That is a policy applied **to** a
//!   replayed tree, and applying it here would make replay unable to answer the question the
//!   policy needs answered — "what does the journal actually say?" A node recorded live with no
//!   exit record replays as [`ReapState::Live`] with [`ReplayedNode::is_unresolved`] true, which
//!   is where the marking will read from.
//! * **It never resolves an unconfirmed reap intent.** §7.2 resolves it by *checking for the
//!   process*, which is I/O and a decision, not a reading.
//! * **It never reads a contract file.** [`ContractPersisted`] says a contract exists and how it
//!   ended; the file is authoritative for its contents (§6.7).
//! * **It does not compact.** §4.3's `snapshot.json` is opportunistic compaction, and it is not
//!   this milestone's work.

use std::collections::HashMap;

use crate::contract::{AgentId, ExitStatus, ProcessExit, ResultStatus, TaskId};
use crate::harness::Harness;
use crate::ir::SrcSeq;
use crate::journal::{
    ContractPersisted, JournalRecord, PermissionDenied, RecordKind, SpawnIntent, WriterId, decode,
};
use crate::node::{NodeState, ReapState};
use crate::root_change::{RootChanged, RootGrant, RootObservation};

/// A contract, as the journal knows it: that it exists, whose it is, and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayedContract {
    pub task_id: TaskId,
    pub requester: AgentId,
    pub status: Option<ResultStatus>,
}

/// One node of the replayed tree.
///
/// The immutable half is the [`SpawnIntent`] itself, held whole rather than copied field by field
/// — and **`Option`al, honestly**: a journal whose head was compacted away, or whose intent record
/// was lost, yields a node marion has records *about* but no identity *for*. Making `harness` a
/// non-optional field would have forced a fabricated default into exactly that case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayedNode {
    pub agent_id: AgentId,
    pub intent: Option<SpawnIntent>,
    /// §6.1 step 7's confirmation arrived.
    pub spawn_confirmed: bool,
    pub harness_version: Option<String>,
    pub model: Option<String>,
    pub pid: Option<i32>,
    /// `Some` iff the intent was resolved by an abort rather than a confirmation.
    pub spawn_aborted: Option<String>,
    pub state: NodeState,
    pub reap_state: ReapState,
    /// Set by a `ReapIntent` with no `ReapConfirmed` yet. §7.2's restart resolution reads this;
    /// replay only reports it.
    pub reap_intent: Option<String>,
    pub exit: Option<ProcessExit>,
    pub contracts: Vec<ReplayedContract>,
    pub denied_permissions: Vec<PermissionDenied>,
    /// §9's root change record, as the journal knows it. **Last record wins**, and `None` means
    /// *the journal says nothing* — which is a third reading beside "marion did not look" and
    /// "marion looked and nothing changed". See [`Self::did_marion_look`].
    ///
    /// One `Option` and one match arm, because replay is total and infallible (see [`replay`]) and
    /// a record that could fail to fold would put a `Result` in the one place §7.4 already fixed
    /// the policy for.
    pub root_change: Option<RootChanged>,
    /// §9's grant, as the journal knew it **before the process started**. See
    /// [`Self::granted_without_a_record`] for the reading it makes possible.
    pub root_grant: Option<RootGrant>,
    /// How many records mentioned this node — the audit handle for "the journal says nothing
    /// more about it than that it started".
    pub records: usize,
}

impl ReplayedNode {
    fn new(agent_id: AgentId) -> Self {
        Self {
            agent_id,
            intent: None,
            spawn_confirmed: false,
            harness_version: None,
            model: None,
            pid: None,
            spawn_aborted: None,
            // Before any state record, a node is `Spawning` — §3.2's first state, and the only one
            // an intent alone justifies.
            state: NodeState::Spawning,
            reap_state: ReapState::Live,
            reap_intent: None,
            exit: None,
            contracts: Vec::new(),
            denied_permissions: Vec::new(),
            root_change: None,
            root_grant: None,
            records: 0,
        }
    }

    /// §7.5: immutable, so this is whatever the intent said and nothing later can move it.
    pub fn parent_id(&self) -> Option<&AgentId> {
        self.intent.as_ref().and_then(|i| i.parent_id.as_ref())
    }

    pub fn harness(&self) -> Option<Harness> {
        self.intent.as_ref().map(|i| i.harness)
    }

    pub fn agent_type(&self) -> Option<&str> {
        self.intent.as_ref().map(|i| i.agent_type.as_str())
    }

    pub fn depth(&self) -> Option<u32> {
        self.intent.as_ref().map(|i| i.depth)
    }

    pub fn task_id(&self) -> Option<&TaskId> {
        self.intent.as_ref().and_then(|i| i.task_id.as_ref())
    }

    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.state.exit_status()
    }

    /// **Is there a measurement behind this node's silence?** (§9, and `8a69f22`'s subject.)
    ///
    /// The question is not "did anything change" — it is whether marion *has a reading at all*.
    /// Before [`RootChanged`] existed, a root that wrote and a root that did not produced the same
    /// nothing, which is the exact byte pattern §11 item 24 describes and `8a69f22` was written to
    /// destroy for children. Three inputs, three answers, and a reader that keys on
    /// [`Self::root_change`] being `Some` would collapse two of them:
    ///
    /// | journal | `root_change` | this |
    /// |---|---|---|
    /// | says nothing | `None` | `false` |
    /// | `NotAttempted` | `Some` | `false` |
    /// | `Failed` | `Some` | `false` |
    /// | `Observed { changed_count: 0 }` | `Some` | `true` |
    ///
    /// **`Failed` is `false`, and that is not an oversight.** An attempt that could not see
    /// produced no delta, so reading it as a look would let a broken `git` masquerade as a clean
    /// bill of health — which is precisely the shape of failure this method exists to name.
    pub fn did_marion_look(&self) -> bool {
        matches!(
            self.root_change.as_ref().map(|c| &c.observation),
            Some(RootObservation::Observed { .. })
        )
    }

    /// **A grant was issued and nothing ever said what came of it** — the run marion did not
    /// survive (§9).
    ///
    /// [`Self::did_marion_look`] answers *is there a measurement*; this answers *was there
    /// something to measure*. The pair distinguishes a root that ran with no tools and produced an
    /// honest empty record from one that was handed `write` on the operator's own checkout and then
    /// disappeared — a distinction the journal could not make at all until the grant was written
    /// before the process rather than after it.
    ///
    /// **A reading, not a policy.** Like [`Self::is_unresolved`], it says what the journal shows;
    /// what to *do* about a root whose grant has no outcome is §7.2's restart question, and replay
    /// deliberately does not answer it.
    pub fn granted_without_a_record(&self) -> bool {
        self.root_grant.is_some() && self.root_change.is_none()
    }

    /// **The node §7.2's `Orphaned` marking will be about**: recorded live, no exit observed, no
    /// reap decided. Named as a question rather than answered as a state, because the answer is a
    /// policy decision taken on restart and this is a reading of the journal.
    pub fn is_unresolved(&self) -> bool {
        !self.state.is_exited()
            && self.reap_state == ReapState::Live
            && self.reap_intent.is_none()
            && self.spawn_aborted.is_none()
    }
}

/// A per-writer ordinal gap: §4.2's `Ordinal` loss detection, applied to marion's own records.
///
/// Distinct from a torn tail. A tail is the end of the file; a gap is a record that was written
/// and is not there, which on an append-only local file means the file was edited or a write was
/// lost — worth reporting rather than papering over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeqGap {
    pub writer: WriterId,
    pub expected: u64,
    pub found: u64,
}

/// Where the intact prefix ended, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Truncation {
    /// The file's last bytes are not newline-terminated: a crash mid-append, or a write that was
    /// cut short. §7.4: *"A truncated final line is discarded on replay."*
    UnterminatedTail { byte_offset: usize, bytes: usize },
    /// A complete line that is not a record. On an append-only file this is corruption rather than
    /// a torn write, so replay stops here rather than skipping — resuming past unexplained bytes
    /// would silently narrate a tree from a file it does not understand.
    Unparsable { byte_offset: usize, line: usize },
}

/// The tree, plus everything replay noticed about the reading itself.
///
/// **A reading, not a snapshot of a call.** [`replay`] is one [`Replay::extend`] over a whole file;
/// a reader tailing a growing file is many `extend`s over the same file's successive tails, and the
/// two must produce the same value. That is why the per-writer expectation map and the last
/// ordering evidence live *here* rather than as locals of [`replay`]: state kept in the function
/// would reset on every chunk, and the loss detection §4.2 asks for would work only when the whole
/// file happened to arrive in one read. See
/// [`Replay::extend`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Replay {
    nodes: Vec<ReplayedNode>,
    index: HashMap<String, usize>,
    /// Records accepted. Not lines in the file — the two differ exactly by [`Replay::truncation`].
    pub records: usize,
    /// Where **this** `extend` stopped, or `None` if it consumed everything offered. Recomputed per
    /// call, because a torn tail is resolved by the next call: reporting a tear that has since been
    /// finished would make a healthy tail look permanently damaged.
    pub truncation: Option<Truncation>,
    pub gaps: Vec<SeqGap>,
    /// The last source-side ordering evidence any record carried — §7.3.3's replay-to-subscribe
    /// seam, and the value `marion_proto::ReplayPoint::src_seq` is built from.
    ///
    /// `None` on every journal marion writes today, and that is §4.2's rule rather than a gap:
    /// marion is the source of its own records and a source has no upstream ordinal to report. A
    /// record carrying none therefore **does not retract** one that did — where marion cannot
    /// detect loss it must not imply otherwise, and overwriting evidence with an absence would be
    /// implying the opposite.
    pub last_src_seq: Option<SrcSeq>,
    /// Each writer's next expected ordinal. Private: it is bookkeeping for [`Replay::gaps`], and a
    /// caller that could set it could suppress the loss signal it exists to raise.
    expected: HashMap<WriterId, u64>,
}

impl Replay {
    /// Nodes in **first-mention order**, which is the journal's own order and therefore the same
    /// on every replay of the same bytes. §9's structural comparison needs a deterministic
    /// sequence, and file order is the only one the journal actually defines.
    pub fn nodes(&self) -> &[ReplayedNode] {
        &self.nodes
    }

    pub fn get(&self, id: &AgentId) -> Option<&ReplayedNode> {
        self.index.get(&id.0).map(|i| &self.nodes[*i])
    }

    /// Nodes with no parent edge — the roots of the forest. A forest, not a tree: one project's
    /// journal accumulates every `marion run` against that project.
    pub fn roots(&self) -> Vec<&ReplayedNode> {
        self.nodes
            .iter()
            .filter(|n| n.parent_id().is_none())
            .collect()
    }

    /// The children of a node, in first-mention order.
    pub fn children(&self, id: &AgentId) -> Vec<&ReplayedNode> {
        self.nodes
            .iter()
            .filter(|n| n.parent_id() == Some(id))
            .collect()
    }

    /// Nodes §7.2's restart marking would consider — recorded live, fate unrecorded. Replay
    /// **reports** them; marking them `Orphaned` is the policy that reads this.
    pub fn unresolved(&self) -> Vec<&ReplayedNode> {
        self.nodes.iter().filter(|n| n.is_unresolved()).collect()
    }

    fn node_mut(&mut self, id: &AgentId) -> &mut ReplayedNode {
        let i = *self.index.entry(id.0.clone()).or_insert_with(|| {
            self.nodes.push(ReplayedNode::new(id.clone()));
            self.nodes.len() - 1
        });
        &mut self.nodes[i]
    }

    fn apply(&mut self, r: JournalRecord) {
        let node = self.node_mut(r.agent_id());
        node.records += 1;
        match r.kind {
            RecordKind::SpawnIntent(i) => {
                // First writer wins: §7.5 makes `parent_id` immutable, and a duplicate intent for
                // one agent id is a bug in the writer, not a re-parenting the tree should follow.
                if node.intent.is_none() {
                    node.intent = Some(i);
                }
            }
            RecordKind::Spawned(s) => {
                node.spawn_confirmed = true;
                node.harness_version = Some(s.harness_version);
                node.model = s.model;
                node.pid = s.pid;
            }
            RecordKind::SpawnAborted(a) => node.spawn_aborted = Some(a.reason),
            RecordKind::StateChanged(s) => {
                // `Exited` is written by the `Exited` record, which carries the `ProcessExit` too.
                // A `StateChanged` naming an exit is accepted all the same — it is what the
                // journal says — but it cannot un-exit a node.
                if !node.state.is_exited() {
                    node.state = s.state;
                }
            }
            RecordKind::Exited(e) => {
                node.state = NodeState::Exited(e.status);
                node.exit = Some(e.exit);
            }
            RecordKind::ReapIntent(i) => node.reap_intent = Some(i.reason),
            RecordKind::ReapConfirmed(_) => node.reap_state = ReapState::ReapedIdle,
            RecordKind::ContractPersisted(c) => {
                let ContractPersisted {
                    task_id,
                    requester,
                    status,
                    ..
                } = c;
                // A contract is written once per run and may be *updated* — §6.7 finalizes it at
                // the terminal transition — so a second record for the same task id replaces the
                // first rather than appearing twice.
                match node.contracts.iter_mut().find(|c| c.task_id == task_id) {
                    Some(existing) => {
                        existing.requester = requester;
                        existing.status = status;
                    }
                    None => node.contracts.push(ReplayedContract {
                        task_id,
                        requester,
                        status,
                    }),
                }
            }
            RecordKind::PermissionDenied(d) => node.denied_permissions.push(d),
            // Last record wins. A root is snapshotted twice in one run and journalled once, so a
            // second record for one node means a *re*-run of the same agent id, which cannot
            // happen, or a rewrite marion made deliberately — either way the later reading is the
            // one that was true last.
            RecordKind::RootChanged(c) => node.root_change = Some(c),
            // Written once, in `prepare`, before the process exists. Overwriting rather than
            // keeping the first has the same justification as the arm above: a second record for
            // one agent id cannot happen in a run, so the later one is the one that was true last.
            RecordKind::RootGrantDecided(g) => node.root_grant = Some(g),
        }
    }

    fn check_seq(&mut self, r: &JournalRecord) {
        let next = self.expected.entry(r.writer.clone()).or_insert(r.seq);
        if r.seq != *next {
            self.gaps.push(SeqGap {
                writer: r.writer.clone(),
                expected: *next,
                found: r.seq,
            });
        }
        *next = r.seq.saturating_add(1);
    }

    /// Fold more journal bytes onto this reading, and answer **how many were consumed**.
    ///
    /// The return value is the length of the intact prefix of `bytes`, which is exactly what a
    /// cursor over a growing file may advance by. Everything after it is a record that is not
    /// finished (the writer is still appending it) or a line replay refuses to guess at, and both
    /// must be offered again rather than skipped — the first because it will complete, the second
    /// because skipping it would narrate a tree from bytes marion does not understand. A caller
    /// therefore never has to reproduce the [`Truncation`] match to know where to stop; there is
    /// one rule and it lives here, so `JournalWatch` and the supervisor's registry cannot drift
    /// apart on it.
    ///
    /// **Total and infallible for the same reasons [`replay`] is**, and adding it does not change
    /// that: any byte string may be offered, an empty one included, and an already-torn reading may
    /// be extended. `bytes` **must** begin at a record boundary — the offset a previous `extend`
    /// consumed to — because there is no framing information for a reader that starts mid-line; a
    /// caller that starts elsewhere gets an [`Truncation::Unparsable`] on its first line rather
    /// than a wrong tree, which is the honest failure of the two available.
    pub fn extend(&mut self, bytes: &[u8]) -> usize {
        self.truncation = None;
        let mut offset = 0usize;
        for (line_no, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
            if line.last() != Some(&b'\n') {
                // No terminator: these bytes are a prefix of a record that was never finished. §7.4.
                if !line.is_empty() {
                    self.truncation = Some(Truncation::UnterminatedTail {
                        byte_offset: offset,
                        bytes: line.len(),
                    });
                }
                break;
            }
            let body = &line[..line.len() - 1];
            // A bare newline is not corruption: it is what the writer emits to close a torn line so
            // a concurrent writer's next record can never be glued onto it.
            if !body.is_empty() {
                match decode(body) {
                    Some(record) => {
                        self.check_seq(&record);
                        if record.src_seq.is_some() {
                            self.last_src_seq = record.src_seq.clone();
                        }
                        self.apply(record);
                        self.records += 1;
                    }
                    None => {
                        self.truncation = Some(Truncation::Unparsable {
                            byte_offset: offset,
                            line: line_no,
                        });
                        break;
                    }
                }
            }
            offset += line.len();
        }
        offset
    }
}

/// Replay a journal's bytes into the tree they record.
///
/// **Total and infallible**: every byte string is a valid input, including an empty one, one that
/// is not UTF-8, and one cut mid-record or mid-multi-byte-character. The result is the longest
/// intact prefix, with [`Replay::truncation`] saying where it ended. There is no error return
/// because there is no failure mode a caller could act on differently — §7.4 already fixes the
/// policy ("a truncated final line is discarded") and a `Result` would only invite a caller to
/// treat a crash-truncated journal, which is the *expected* state after a SIGKILL, as a fault.
///
/// One [`Replay::extend`] over the whole file. A reader tailing a growing one calls `extend`
/// repeatedly instead, and gets the same value — which is the property that lets the supervisor's
/// live registry and this function be the same reading rather than two.
pub fn replay(bytes: &[u8]) -> Replay {
    let mut out = Replay::default();
    out.extend(bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::SystemTime;
    use crate::ir::Provenance;
    use crate::journal::{Exited, ReapConfirmed, ReapIntent, Spawned, StateChanged, encode};

    fn record(seq: u64, kind: RecordKind) -> JournalRecord {
        JournalRecord {
            writer: WriterId("w".into()),
            seq,
            ts: SystemTime::from_unix_millis(1_785_625_628_619),
            mono_ns: seq,
            provenance: Provenance::marion(),
            src_seq: None,
            kind,
        }
    }

    fn id(s: &str) -> AgentId {
        AgentId(s.into())
    }

    fn bytes(records: &[JournalRecord]) -> Vec<u8> {
        records.iter().flat_map(|r| encode(r).unwrap()).collect()
    }

    /// A root, a child under it, the child's contract, and both terminals — the M1 shape.
    fn m1_journal() -> Vec<JournalRecord> {
        let mut seq = 0;
        let mut next = |k| {
            let r = record(seq, k);
            seq += 1;
            r
        };
        vec![
            next(RecordKind::SpawnIntent(SpawnIntent {
                agent_id: id("root"),
                parent_id: None,
                agent_type: "claude".into(),
                harness: Harness::ClaudeCode,
                depth: 0,
                task_id: None,
            })),
            next(RecordKind::Spawned(Spawned {
                agent_id: id("root"),
                harness_version: "2.1.220".into(),
                model: None,
                pid: Some(11),
            })),
            next(RecordKind::SpawnIntent(SpawnIntent {
                agent_id: id("child"),
                parent_id: Some(id("root")),
                agent_type: "codex-impl".into(),
                harness: Harness::Codex,
                depth: 1,
                task_id: Some(TaskId("t-1".into())),
            })),
            next(RecordKind::Spawned(Spawned {
                agent_id: id("child"),
                harness_version: "0.9.0".into(),
                model: None,
                pid: Some(12),
            })),
            next(RecordKind::StateChanged(StateChanged {
                agent_id: id("child"),
                state: NodeState::Running,
            })),
            next(RecordKind::Exited(Exited {
                agent_id: id("child"),
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "clean exit".into(),
                },
            })),
            next(RecordKind::ContractPersisted(ContractPersisted {
                agent_id: id("child"),
                task_id: TaskId("t-1".into()),
                requester: id("root"),
                status: Some(ExitStatus::Ok),
            })),
            next(RecordKind::Exited(Exited {
                agent_id: id("root"),
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "clean exit".into(),
                },
            })),
        ]
    }

    #[test]
    fn replay_reconstructs_nodes_edges_terminals_and_contracts() {
        // §9's M2 criterion, as an assertion: same nodes, same parent edges, same terminal states,
        // same contracts.
        let r = replay(&bytes(&m1_journal()));
        assert_eq!(r.records, 8);
        assert_eq!(r.truncation, None);
        assert!(r.gaps.is_empty());

        assert_eq!(
            r.nodes()
                .iter()
                .map(|n| n.agent_id.0.as_str())
                .collect::<Vec<_>>(),
            ["root", "child"],
            "first-mention order, which is the journal's own"
        );
        assert_eq!(
            r.roots()
                .iter()
                .map(|n| n.agent_id.0.as_str())
                .collect::<Vec<_>>(),
            ["root"]
        );
        assert_eq!(
            r.children(&id("root"))
                .iter()
                .map(|n| n.agent_id.0.as_str())
                .collect::<Vec<_>>(),
            ["child"]
        );

        let child = r.get(&id("child")).unwrap();
        assert_eq!(child.parent_id(), Some(&id("root")));
        assert_eq!(child.harness(), Some(Harness::Codex));
        assert_eq!(child.agent_type(), Some("codex-impl"));
        assert_eq!(child.depth(), Some(1));
        assert_eq!(child.state, NodeState::Exited(ExitStatus::Ok));
        assert_eq!(child.exit_status(), Some(ExitStatus::Ok));
        assert_eq!(child.exit.as_ref().unwrap().code, Some(0));
        assert_eq!(child.harness_version.as_deref(), Some("0.9.0"));
        assert!(child.spawn_confirmed);
        assert_eq!(
            child.contracts,
            vec![ReplayedContract {
                task_id: TaskId("t-1".into()),
                requester: id("root"),
                status: Some(ExitStatus::Ok),
            }]
        );
        assert!(r.unresolved().is_empty(), "both nodes exited on the record");
    }

    #[test]
    fn a_live_node_with_no_exit_record_is_where_orphaned_will_come_from() {
        // §7.2. Replay must represent it faithfully and must NOT mark it — the marking is a
        // restart policy applied to this reading, not part of the reading.
        let mut j = m1_journal();
        j.retain(|r| !matches!(&r.kind, RecordKind::Exited(e) if e.agent_id == id("root")));
        let r = replay(&bytes(&j));
        let root = r.get(&id("root")).unwrap();
        assert_eq!(root.state, NodeState::Spawning);
        assert_eq!(
            root.reap_state,
            ReapState::Live,
            "replay never produces Orphaned"
        );
        assert!(root.is_unresolved());
        assert_eq!(
            r.unresolved()
                .iter()
                .map(|n| n.agent_id.0.as_str())
                .collect::<Vec<_>>(),
            ["root"]
        );
    }

    #[test]
    fn a_reap_intent_replays_unconfirmed_and_a_confirmation_replays_reaped() {
        let mut j = m1_journal();
        let n = j.len() as u64;
        j.push(record(
            n,
            RecordKind::ReapIntent(ReapIntent {
                agent_id: id("child"),
                reason: "idle memory reclaim".into(),
            }),
        ));
        let r = replay(&bytes(&j));
        let c = r.get(&id("child")).unwrap();
        assert_eq!(c.reap_intent.as_deref(), Some("idle memory reclaim"));
        assert_eq!(
            c.reap_state,
            ReapState::Live,
            "§7.2 resolves an unconfirmed intent by checking for the process, which replay does not do"
        );

        j.push(record(
            n + 1,
            RecordKind::ReapConfirmed(ReapConfirmed {
                agent_id: id("child"),
            }),
        ));
        let r = replay(&bytes(&j));
        assert_eq!(
            r.get(&id("child")).unwrap().reap_state,
            ReapState::ReapedIdle
        );
    }

    #[test]
    fn an_aborted_spawn_is_not_an_unresolved_node() {
        // §6.1 step 8 / §7.2: marion knows what it intended for this process, so it must never
        // look like a node marion lost.
        let j = vec![
            record(
                0,
                RecordKind::SpawnIntent(SpawnIntent {
                    agent_id: id("a"),
                    parent_id: None,
                    agent_type: "claude".into(),
                    harness: Harness::ClaudeCode,
                    depth: 0,
                    task_id: None,
                }),
            ),
            record(
                1,
                RecordKind::SpawnAborted(crate::journal::SpawnAborted {
                    agent_id: id("a"),
                    reason: "the bridge never handshook within 30 s".into(),
                }),
            ),
        ];
        let r = replay(&bytes(&j));
        assert!(!r.get(&id("a")).unwrap().is_unresolved());
    }

    #[test]
    fn a_node_whose_intent_is_missing_has_no_fabricated_identity() {
        let j = vec![record(
            0,
            RecordKind::Spawned(Spawned {
                agent_id: id("orphan-record"),
                harness_version: "1".into(),
                model: None,
                pid: None,
            }),
        )];
        let r = replay(&bytes(&j));
        let n = r.get(&id("orphan-record")).unwrap();
        assert_eq!(n.harness(), None);
        assert_eq!(n.depth(), None);
        assert_eq!(n.parent_id(), None);
        assert!(n.spawn_confirmed);
    }

    #[test]
    fn a_contract_record_updates_rather_than_duplicates() {
        let mut j = m1_journal();
        let n = j.len() as u64;
        j.push(record(
            n,
            RecordKind::ContractPersisted(ContractPersisted {
                agent_id: id("child"),
                task_id: TaskId("t-1".into()),
                requester: id("root"),
                status: Some(ExitStatus::Failed),
            }),
        ));
        let r = replay(&bytes(&j));
        let c = r.get(&id("child")).unwrap();
        assert_eq!(c.contracts.len(), 1);
        assert_eq!(c.contracts[0].status, Some(ExitStatus::Failed));
    }

    #[test]
    fn an_ordinal_gap_is_reported_rather_than_hidden() {
        // §4.2's `Ordinal` form, applied to marion's own per-writer sequence.
        let j = vec![
            record(
                0,
                RecordKind::ReapConfirmed(ReapConfirmed { agent_id: id("a") }),
            ),
            record(
                2,
                RecordKind::ReapConfirmed(ReapConfirmed { agent_id: id("a") }),
            ),
        ];
        let r = replay(&bytes(&j));
        assert_eq!(
            r.gaps,
            vec![SeqGap {
                writer: WriterId("w".into()),
                expected: 1,
                found: 2
            }]
        );
        assert_eq!(r.records, 2, "a gap is reported, not a reason to stop");
    }

    #[test]
    fn two_writers_interleave_without_being_read_as_a_gap() {
        let a = JournalRecord {
            writer: WriterId("a".into()),
            ..record(
                0,
                RecordKind::ReapConfirmed(ReapConfirmed { agent_id: id("x") }),
            )
        };
        let b = JournalRecord {
            writer: WriterId("b".into()),
            ..record(
                0,
                RecordKind::ReapConfirmed(ReapConfirmed { agent_id: id("y") }),
            )
        };
        let a1 = JournalRecord {
            seq: 1,
            ..a.clone()
        };
        let r = replay(&bytes(&[a, b, a1]));
        assert!(r.gaps.is_empty(), "sequences are per-writer, not global");
        assert_eq!(r.records, 3);
    }

    #[test]
    fn an_empty_journal_replays_to_an_empty_tree() {
        let r = replay(b"");
        assert_eq!(r.records, 0);
        assert!(r.nodes().is_empty());
        assert_eq!(r.truncation, None);
    }

    /// A root's journal, with whatever change record it was given. `None` is the third case: a
    /// journal from a build that had no such record, or a run whose record was lost.
    fn root_journal(change: Option<RootObservation>) -> Vec<u8> {
        let mut j = vec![record(
            0,
            RecordKind::SpawnIntent(SpawnIntent {
                agent_id: id("root"),
                parent_id: None,
                agent_type: "claude".into(),
                harness: Harness::ClaudeCode,
                depth: 0,
                task_id: None,
            }),
        )];
        if let Some(observation) = change {
            j.push(record(
                1,
                RecordKind::RootChanged(RootChanged {
                    agent_id: id("root"),
                    base_commit: Some(crate::contract::Oid("a".repeat(40))),
                    head_at_exit: Some(crate::contract::Oid("a".repeat(40))),
                    observation,
                }),
            ));
        }
        bytes(&j)
    }

    /// **NC-4 — the three readings, through replay.**
    ///
    /// *The journal is silent* / *marion did not look, here is why* / *marion looked and nothing
    /// changed*. That triple is what the whole record exists to produce, and the assertion is
    /// stated on [`ReplayedNode::did_marion_look`] rather than only on the `Option`, because a
    /// reader keying on `Some` gets two of the three right and the important one wrong.
    #[test]
    fn replay_tells_a_silent_journal_from_a_refusal_to_look_from_a_clean_reading() {
        let clean = RootObservation::Observed {
            pre_tree: crate::contract::Oid("b".repeat(40)),
            post_tree: crate::contract::Oid("b".repeat(40)),
            changed_count: 0,
            dirty_at_launch: 0,
            diff_bytes: 0,
            scope_violation_count: 0,
            ignored_not_measured: Some(0),
        };
        let cases = [
            ("the journal says nothing", None, false),
            (
                "marion did not look",
                Some(RootObservation::NotAttempted {
                    reason: crate::root_change::Reason::new("not a git worktree"),
                }),
                false,
            ),
            (
                "marion looked and could not see",
                Some(RootObservation::Failed {
                    reason: crate::root_change::Reason::new("git: command not found"),
                }),
                false,
            ),
            ("marion looked and nothing changed", Some(clean), true),
        ];
        let mut seen = Vec::new();
        for (label, observation, looked) in cases {
            let present = observation.is_some();
            let r = replay(&root_journal(observation));
            let n = r.get(&id("root")).unwrap();
            assert_eq!(n.root_change.is_some(), present, "{label}");
            assert_eq!(
                n.did_marion_look(),
                looked,
                "{label}: pre-`8a69f22` a root that wrote and a root that did not produced the \
                 same empty `changed_paths` and no record at all; collapsing these readings \
                 re-creates exactly that byte pattern (§11 item 24)"
            );
            seen.push(n.root_change.clone());
        }
        // …and the three are not merely differently *labelled*: no two of the four readings are
        // the same value, so nothing downstream can conflate them by accident.
        for i in 0..seen.len() {
            for k in i + 1..seen.len() {
                assert_ne!(
                    seen[i], seen[k],
                    "readings {i} and {k} are indistinguishable"
                );
            }
        }
    }

    /// The fold is last-record-wins, like `ContractPersisted`'s update — and one `Option`, so a
    /// second record cannot accumulate a list nobody reads.
    #[test]
    fn a_second_change_record_replaces_the_first() {
        let mut j = vec![record(
            0,
            RecordKind::RootChanged(RootChanged {
                agent_id: id("root"),
                base_commit: None,
                head_at_exit: None,
                observation: RootObservation::NotAttempted {
                    reason: crate::root_change::Reason::new("first"),
                },
            }),
        )];
        j.push(record(
            1,
            RecordKind::RootChanged(RootChanged {
                agent_id: id("root"),
                base_commit: None,
                head_at_exit: None,
                observation: RootObservation::Failed {
                    reason: crate::root_change::Reason::new("second"),
                },
            }),
        ));
        let r = replay(&bytes(&j));
        assert!(matches!(
            r.get(&id("root"))
                .unwrap()
                .root_change
                .as_ref()
                .unwrap()
                .observation,
            RootObservation::Failed { .. }
        ));
    }

    /// **NC-8 — replay stays total across a change record it cannot read.**
    ///
    /// Three journals: one whose `RootChanged` line is cut mid-record, one whose `observation` tag
    /// is a name no build ever wrote, and one that is valid. No panic and no `Result` — §7.4 fixes
    /// the policy already, and the reading is reported as [`Truncation`] exactly as
    /// `registry.rs`'s two variants specify.
    #[test]
    fn a_torn_or_garbled_change_record_ends_the_prefix_without_a_panic() {
        let valid = root_journal(Some(RootObservation::NotAttempted {
            reason: crate::root_change::Reason::new("not a git worktree"),
        }));
        assert_eq!(replay(&valid).truncation, None);
        assert!(
            replay(&valid)
                .get(&id("root"))
                .unwrap()
                .root_change
                .is_some()
        );

        // A crash mid-append: the last line has no terminator. §7.4 discards it, and the node
        // keeps the identity the intact prefix gave it.
        let torn = &valid[..valid.len() - 20];
        let r = replay(torn);
        assert!(
            matches!(r.truncation, Some(Truncation::UnterminatedTail { .. })),
            "{:?}",
            r.truncation
        );
        assert!(
            !r.get(&id("root")).unwrap().did_marion_look(),
            "a record that was never finished must not be read as a reading"
        );

        // A complete line naming a variant that does not exist. On an append-only file that is
        // corruption rather than a torn write, so the prefix ends here rather than skipping it.
        let garbled = String::from_utf8_lossy(&valid)
            .replace("NotAttempted", "SomethingNobodyWrote")
            .into_bytes();
        let r = replay(&garbled);
        assert!(
            matches!(r.truncation, Some(Truncation::Unparsable { line: 1, .. })),
            "{:?}",
            r.truncation
        );
        assert_eq!(r.records, 1, "the intact prefix is the intent alone");
        assert!(!r.get(&id("root")).unwrap().did_marion_look());
    }

    /// **A reading built in chunks is the same reading, and it detects a gap at a chunk seam.**
    ///
    /// This is the property a *tailing* reader needs and a one-shot one never exercises. Before
    /// [`Replay::extend`], the per-writer expectation map was a local inside [`replay`], so a
    /// caller polling a growing file called `replay` once per chunk and reset the map every time —
    /// the first record of every chunk became its own baseline and a gap **at a chunk boundary was
    /// undetectable**. `watch.rs` polls exactly that way. A gap that only shows up when the whole
    /// file happens to arrive in one read is not a gap check.
    #[test]
    fn a_reading_folded_in_chunks_equals_the_whole_and_still_sees_a_gap_at_the_seam() {
        let j = m1_journal();
        let whole = bytes(&j);

        // Same bytes, one record at a time.
        let mut chunked = Replay::default();
        let mut at = 0usize;
        for r in &j {
            let line = encode(r).unwrap();
            let n = chunked.extend(&whole[at..at + line.len()]);
            assert_eq!(n, line.len(), "a whole record is wholly consumed");
            at += line.len();
        }
        assert_eq!(chunked, replay(&whole), "one fold, however it is fed");

        // …and the gap check survives the seam it used to be blind to.
        let mut r = Replay::default();
        r.extend(&bytes(&[record(
            0,
            RecordKind::ReapConfirmed(ReapConfirmed { agent_id: id("a") }),
        )]));
        assert!(r.gaps.is_empty());
        r.extend(&bytes(&[record(
            2,
            RecordKind::ReapConfirmed(ReapConfirmed { agent_id: id("a") }),
        )]));
        assert_eq!(
            r.gaps,
            vec![SeqGap {
                writer: WriterId("w".into()),
                expected: 1,
                found: 2
            }],
            "the writer's ordinal is remembered across polls, or a tailing reader cannot see loss"
        );
    }

    /// **Extend consumes the intact prefix and no more**, which is what makes a caller's cursor
    /// safe: the torn bytes are re-offered on the next call and read once, when they are finished.
    #[test]
    fn extend_consumes_only_the_intact_prefix_so_a_torn_tail_is_read_once_when_it_completes() {
        let whole = bytes(&m1_journal());
        let cut = whole.len() - 20;
        let mut r = Replay::default();
        let n = r.extend(&whole[..cut]);
        assert!(
            matches!(r.truncation, Some(Truncation::UnterminatedTail { .. })),
            "{:?}",
            r.truncation
        );
        assert!(n < cut, "the torn tail is not consumed");
        assert_eq!(r.records, 7, "seven whole records, the eighth unfinished");

        // The caller re-offers from its cursor; the eighth record arrives exactly once.
        let n2 = r.extend(&whole[n..]);
        assert_eq!(n + n2, whole.len());
        assert_eq!(r.truncation, None, "a resolved tear is no longer reported");
        assert_eq!(r, replay(&whole), "and the reading is the whole reading");
    }

    /// The last ordering evidence any record carried, which is §7.3.3's seam value. `None` on
    /// every journal marion writes today (§4.2: marion is the source and has no upstream ordinal),
    /// so this is stated as *what it would be* rather than assumed — and a record with none must
    /// never erase one that had it.
    #[test]
    fn the_last_source_ordering_evidence_is_remembered_and_never_erased_by_a_record_without_one() {
        let r = replay(&bytes(&m1_journal()));
        assert_eq!(r.last_src_seq, None, "no marion writer emits one");

        let evidence = SrcSeq::Predecessor(crate::ir::EventId("u-9".into()));
        let mut with = record(
            0,
            RecordKind::ReapConfirmed(ReapConfirmed { agent_id: id("a") }),
        );
        with.src_seq = Some(evidence.clone());
        let without = record(
            1,
            RecordKind::ReapConfirmed(ReapConfirmed { agent_id: id("a") }),
        );
        let r = replay(&bytes(&[with, without]));
        assert_eq!(
            r.last_src_seq,
            Some(evidence),
            "§4.2: a record with no evidence reports no ordering — it does not retract one"
        );
    }

    #[test]
    fn a_blank_line_is_not_corruption() {
        // The writer emits one to close a torn line; it must not stop replay.
        let mut b = bytes(&m1_journal());
        b.extend_from_slice(b"\n");
        b.extend_from_slice(
            &encode(&record(
                9,
                RecordKind::ReapConfirmed(ReapConfirmed {
                    agent_id: id("child"),
                }),
            ))
            .unwrap(),
        );
        let r = replay(&b);
        assert_eq!(r.truncation, None);
        assert_eq!(
            r.get(&id("child")).unwrap().reap_state,
            ReapState::ReapedIdle
        );
    }
}
