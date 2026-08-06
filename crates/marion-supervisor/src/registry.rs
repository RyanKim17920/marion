//! The registry, **running** (design §4.3, §5.1, §9's M2 substrate; plan item 3.3).
//!
//! `marion_core::registry::replay` is a pure fold from bytes to a tree. This is the thing that
//! *has* one: it boots by replaying a project's `journal.jsonl`, and then stays current by
//! following the same file as it grows. Everything about the tree is still `marion-core`'s — this
//! module owns a cursor, a clock-free poll, and the honest answer to *"is what I am serving still
//! what the journal says?"*
//!
//! # Why it tails the journal instead of being updated where marion emits
//!
//! The alternative is real and was rejected on a structural ground, not a taste one. `marion run`
//! and each `marion-supervisor mcp` bridge are **separate processes** (`journal.rs` opens its
//! concurrency argument with exactly this), and a child's whole lifecycle — `SpawnIntent`,
//! `Spawned`, `Exited`, `ContractPersisted` — is written by the *bridge's* process, which is where
//! a child is actually driven. A registry updated at the emission sites inside one process would
//! therefore not be *at risk* of drifting from the journal about the other process's nodes; it
//! would be **unable to see them at all**. Tailing is not the slower correct option, it is the only
//! one, and it also happens to be the one with a single derivation — the drift `run.rs` names as
//! "one derivation, two names" is precisely what a second update path would create.
//!
//! **The latency cost is smaller than it reads.** §4.3's ~50 ms group commit delays the *fsync*,
//! not the `write(2)`: the bytes are in the page cache and visible to another process the moment
//! the writer returns, which `journal.rs`'s own barrier test asserts in as many words ("still
//! readable immediately, because the bytes are in the file either way"). What a follower pays is
//! its poll interval, and re-parsing only the bytes that are new.
//!
//! # Its relationship to `watch.rs`
//!
//! They are not merged, and the reason is that their cursors start at **opposite ends by design**:
//! `JournalWatch::at_end` deliberately begins past everything already written, because one run's
//! view must not narrate the project's history; a registry begins at byte 0, because §9's criterion
//! is the whole forest the journal records. One cursor cannot serve both, and folding a view's
//! ignore-the-root rule and event vocabulary into the authority would put rendering policy inside
//! the registry.
//!
//! What they genuinely shared was one rule — *advance only to the intact prefix* — written twice.
//! That now lives once, in [`marion_core::registry::Replay::extend`], which both call. Promoting it
//! there also fixed a hole a viewer could not see: the per-writer ordinal map used to be a local of
//! `replay`, so a reader polling a growing file reset it every chunk and could never detect a lost
//! record **at a chunk boundary**. A tailing registry is exactly a reader that only ever sees
//! chunks.
//!
//! # Locking
//!
//! Three layers, and only the middle one is a lock.
//!
//! * **The file: none.** Append-only means bytes already read never change, so a reader needs no
//!   coordination at all; the writers' story is `O_APPEND` atomicity under `MAX_RECORD_BYTES` and
//!   is unchanged (`journal.rs`).
//! * **In-process: one `Mutex`**, and every acquisition takes a **poisoned** lock rather than
//!   panicking — see [`LiveRegistry::read`].
//! * **Across processes: none, and none is owed here.** Two supervisors over one project is
//!   prevented by §5.7's start race (bind-then-publish or a lockfile, *"which one marion uses is
//!   not settled here"*), which is the socket's problem and not this module's.
//!
//! # What it deliberately does not answer yet
//!
//! * **`marion_proto::NodeSummary`.** Two of its fields have no source in the journal at all:
//!   `timeout` (§3.1's bound — `SpawnIntent` carries no timeout) and `name` (there is no
//!   `node/rename`). Building one means fabricating both, which is the thing
//!   `marion-core`'s `ReplayedNode` refuses by making `intent` an `Option`.
//! * **`marion_proto::AttachMode`.** Its `ResubscribeFrom` arm asserts that *the supervisor still
//!   holds the channel* (§6.2, §7.3.3) — live supervisor state, which no journal can report.
//!   Deriving it from replay would hand a re-attaching client a live-node verdict for a node whose
//!   channel nobody holds, which is the one answer §7.3.3 cannot survive being wrong about.
//! # §7.2's restart marking, and where it happens
//!
//! `marion-core`'s replay is explicit that `Live` → `Orphaned` is not replay's job. It is
//! [`crate::restart`]'s, and [`Registry::boot_path`] runs it **once, on the boot tree, before the
//! first poll** — see the comment there for why that instant and no other. What this module owns is
//! the timing; the judgement, and everything it refuses to decide, is argued in `restart.rs`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use marion_core::paths::ProjectDir;
use marion_core::registry::{Replay, SeqGap, Truncation};
use marion_proto::ReplayPoint;

use crate::journal::JournalError;

/// Whether the tree being served is still the journal's, **and whether the registry can still tell**.
///
/// Four readings, not two, because the collapses are the failure. Fold `Unreadable` into "nothing
/// new" and a journal that vanished is indistinguishable from one with nothing to say; fold
/// `Stopped` into it as well and a registry that has stopped following is indistinguishable from a
/// tree that stopped changing. Both collapses serve a stale tree as a current one, which is exactly
/// what a registry exists not to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Reading the file, current with its intact prefix. This is also what "nothing new" looks
    /// like, which is correct: a quiet journal is not a fault, and [`Registry::read_point`] is what
    /// distinguishes quiet from advancing.
    Following,
    /// The last poll could not read the file. **Transient by construction** — the next poll retries
    /// and a journal that comes back is followed again — but it is never silent, because the
    /// registry is stale for as long as it holds.
    Unreadable { reason: String },
    /// The registry stopped following, for good, and why. Reached by corruption (a complete line
    /// that is not a record) or by the file getting shorter. A registry that goes quiet on its own
    /// is the failure `watch.rs` was written to remove, one layer up: here the stale tree would be
    /// served as an authority rather than merely displayed.
    Stopped { reason: String },
}

/// A project's journal, replayed and then followed.
///
/// Poll-driven and clock-free, so it is testable with no thread and no timer; [`LiveRegistry`] is
/// the thing that drives it.
#[derive(Debug, Clone)]
pub struct Registry {
    path: PathBuf,
    tree: Replay,
    /// Bytes folded in. Always a record boundary — the intact prefix and no further.
    offset: u64,
    boot: ReplayPoint,
    /// Gaps the boot read found, as a count into [`Replay::gaps`]. A count rather than a copy
    /// because the list only grows and the boot's share of it is its prefix.
    boot_gaps: usize,
    booted_complete: bool,
    status: Status,
    polls: u64,
    /// Whether the file has ever been opened. What separates *"this project has not run yet"* —
    /// a journal that does not exist, which is an empty tree — from *"the journal I was reading is
    /// gone"*, which is [`Status::Unreadable`].
    seen_file: bool,
    /// §7.2's restart verdicts for the boot tree. See [`Self::restart_marks`].
    restart_marks: Vec<crate::restart::Marked>,
}

impl Registry {
    /// Boot by replaying the project's journal.
    pub fn boot(project: &ProjectDir) -> Result<Self, JournalError> {
        Self::boot_path(&project.journal())
    }

    /// The same, naming the file directly.
    ///
    /// **A missing journal is an empty tree; an unreadable one is an error.** The same split
    /// `journal::read` already makes, and for the sharper reason here: a project that has never run
    /// has no file and no nodes, but answering "no nodes" to a permission or hardware fault would
    /// be a registry claiming marion has nothing running — the one claim it least may make wrongly.
    ///
    /// **A corrupt line is neither.** It boots, serving the intact prefix, with
    /// [`Status::Stopped`] and [`Self::booted_complete`] false: refusing to boot would take a
    /// supervisor down over a file it can mostly read while real agent processes are running, and
    /// booting silently would present a prefix as the tree.
    pub fn boot_path(path: &Path) -> Result<Self, JournalError> {
        let bytes = match std::fs::read(path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let seen_file = bytes.is_some();
        let bytes = bytes.unwrap_or_default();
        let mut tree = Replay::default();
        let consumed = tree.extend(&bytes);
        // **§7.2's restart marking, here and nowhere else.** Boot is the one moment the distinction
        // is available: everything in `tree` at this instant is what the journal recorded *before
        // this supervisor existed*, so a node still `Live` is by definition one whose fate this
        // supervisor has no record of deciding. Every node folded in by a later [`Self::poll`] is
        // this supervisor's own contemporary and must never be marked — which is why the pass runs
        // before the first poll rather than being a filter over the tree.
        let restart_marks = crate::restart::apply(&mut tree);
        let status = match &tree.truncation {
            Some(Truncation::Unparsable { .. }) => Status::Stopped {
                reason: unparsable_reason(path, consumed as u64),
            },
            // A torn final line is the expected state after a crash (§7.4) and the expected state
            // of a file another process is appending to. Nothing to report; it is re-read.
            Some(Truncation::UnterminatedTail { .. }) | None => Status::Following,
        };
        Ok(Self {
            path: path.to_path_buf(),
            boot: point(&tree),
            boot_gaps: tree.gaps.len(),
            booted_complete: tree.gaps.is_empty() && !matches!(status, Status::Stopped { .. }),
            offset: consumed as u64,
            tree,
            status,
            polls: 0,
            seen_file,
            restart_marks,
        })
    }

    /// **What §7.2's restart pass concluded about this boot's tree**, including the classes it
    /// refused to decide.
    ///
    /// Carried rather than re-derivable, and for the same reason as [`Self::boot_gaps`]: once the
    /// tree has been marked, an `Orphaned` node is no longer `is_unresolved`, so asking the tree
    /// again would answer "nothing was marked". More importantly the *refusals* —
    /// [`crate::restart::Marking::ReapIntentUnresolved`] above all — leave the tree byte-identical
    /// to what replay produced, so they exist in this list or nowhere.
    pub fn restart_marks(&self) -> &[crate::restart::Marked] {
        &self.restart_marks
    }

    /// The tree, as the journal records it.
    pub fn tree(&self) -> &Replay {
        &self.tree
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// **What this registry's own boot read** — §7.3.3's seam, as a value rather than an
    /// implication. *"Replay to the journal's own read point, then subscribe from there"* is only a
    /// procedure if the read point is something the registry can hand over; a supervisor that had
    /// to say "wherever I got to" could not be re-attached to correctly.
    pub fn boot_point(&self) -> &ReplayPoint {
        &self.boot
    }

    /// The read point **now**, which moves as the tail is folded in. Distinct from
    /// [`Self::boot_point`] on purpose: a client attaching mid-life needs where the registry is,
    /// and an audit of a restart needs where it started.
    pub fn read_point(&self) -> ReplayPoint {
        point(&self.tree)
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    /// Polls attempted since boot. With [`Self::read_point`], this is what separates *"the journal
    /// has nothing new"* from *"nobody is reading the journal"* — two states with an identical tree.
    pub fn polls(&self) -> u64 {
        self.polls
    }

    /// The records that were written and are not in the file, as of the boot read (§4.2's
    /// `Ordinal` loss). **Reported rather than repaired**: a tree replayed over a hole can be
    /// structurally identical to one replayed clean — a lost `Spawned` for a node that also has an
    /// `Exited` moves nothing visible — so if this were not carried separately, a partial tree
    /// would present as a complete one, which is this repo's documented failure class.
    pub fn boot_gaps(&self) -> &[SeqGap] {
        &self.tree.gaps[..self.boot_gaps]
    }

    /// Whether the boot read can vouch for the tree it produced: no lost ordinals, and no line it
    /// refused to read. `true` for a project that has never run — nothing read, nothing missing.
    pub fn booted_complete(&self) -> bool {
        self.booted_complete
    }

    /// Fold in whatever has been appended since the last poll, and answer how many **records** that
    /// was. Zero is the overwhelmingly common answer and costs one `open` and one `stat`.
    ///
    /// Never fails and never panics: every way this can go wrong is a [`Status`], because a
    /// registry that returned an error would hand its caller a decision §5.7 has already made —
    /// a supervisor with live nodes keeps running.
    pub fn poll(&mut self) -> usize {
        self.polls += 1;
        if matches!(self.status, Status::Stopped { .. }) {
            return 0;
        }
        let fresh = match self.read_new() {
            Ok(None) => {
                // A read that succeeded and found nothing **clears** a previous `Unreadable`. It
                // has to: `Unreadable` is a claim that the tree may be stale, and leaving it set
                // after marion has looked and found the file complete would keep asserting a
                // staleness that is over — the mirror of the collapse this enum exists to prevent.
                self.status = Status::Following;
                return 0;
            }
            Ok(Some(bytes)) => bytes,
            Err(status) => {
                self.status = status;
                return 0;
            }
        };
        let before = self.tree.records;
        self.offset += self.tree.extend(&fresh) as u64;
        self.status = match &self.tree.truncation {
            Some(Truncation::Unparsable { .. }) => Status::Stopped {
                reason: unparsable_reason(&self.path, self.offset),
            },
            Some(Truncation::UnterminatedTail { .. }) | None => Status::Following,
        };
        self.tree.records - before
    }

    /// The bytes past the cursor. `Ok(None)` is "nothing new"; `Err` carries the status that
    /// explains why there was no reading at all.
    fn read_new(&mut self) -> Result<Option<Vec<u8>>, Status> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            // A journal that does not exist *yet* is a project that has not run, not a fault. One
            // that stops existing after the registry read it is the opposite, and the two are the
            // same `NotFound`, so `seen_file` is what tells them apart.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !self.seen_file => {
                return Ok(None);
            }
            Err(e) => return Err(unreadable(&self.path, &e)),
        };
        self.seen_file = true;
        let len = f.metadata().map_err(|e| unreadable(&self.path, &e))?.len();
        if len == self.offset {
            return Ok(None);
        }
        if len < self.offset {
            // An append-only file never gets shorter. This one is not the file the registry booted
            // against, and an authority may not keep serving a tree from a file it no longer
            // recognises — where `watch.rs` may show less, this must stop and say so.
            return Err(Status::Stopped {
                reason: format!(
                    "{} is {} bytes, shorter than the {} marion had already read; an append-only \
                     journal never shrinks, so this is not the file the registry booted against \
                     and it has stopped following it",
                    self.path.display(),
                    len,
                    self.offset
                ),
            });
        }
        f.seek(SeekFrom::Start(self.offset))
            .map_err(|e| unreadable(&self.path, &e))?;
        let mut buf = Vec::with_capacity((len - self.offset) as usize);
        f.take(len - self.offset)
            .read_to_end(&mut buf)
            .map_err(|e| unreadable(&self.path, &e))?;
        Ok(Some(buf))
    }
}

/// §7.3.3's point, built from the reading rather than remembered alongside it.
fn point(tree: &Replay) -> ReplayPoint {
    ReplayPoint {
        records: tree.records as u64,
        src_seq: tree.last_src_seq.clone(),
    }
}

fn unreadable(path: &Path, e: &std::io::Error) -> Status {
    Status::Unreadable {
        reason: format!(
            "marion could not read {} this poll ({e}); the tree it is serving is as of its last \
             successful read and will be current again when the file is readable",
            path.display()
        ),
    }
}

fn unparsable_reason(path: &Path, offset: u64) -> String {
    format!(
        "{} carries a complete line at byte {offset} that is not a journal record; on an \
         append-only file that is corruption rather than a torn write, so marion stopped \
         following it there rather than narrating a tree from bytes it does not understand. The \
         nodes read before that point are still what the journal says.",
        path.display()
    )
}

/// A [`Registry`] kept current by a thread of its own, shared with whoever reads it.
///
/// This is the *running* half of plan item 3.3: a registry nobody has to remember to poll. No
/// socket and no handler is built on it — those are later changes — so today its callers are its
/// tests, which is what a substrate looks like before the thing it carries lands.
pub struct LiveRegistry {
    inner: Arc<Mutex<Registry>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LiveRegistry {
    /// Start following, polling every `interval`.
    ///
    /// **The loop reads the stop flag *before* its poll and returns *after* it**, which is the same
    /// rule `bin/marion.rs`'s `follow_journal` is written to and for the same reason: records
    /// written in the last moments before a shutdown are exactly the ones a run cares about, and a
    /// loop that returned on the flag before polling would drop them and end the reading on a lie.
    pub fn follow(registry: Registry, interval: Duration) -> Self {
        let inner = Arc::new(Mutex::new(registry));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let inner = Arc::clone(&inner);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                loop {
                    let done = stop.load(Ordering::SeqCst);
                    lock(&inner).poll();
                    if done {
                        return;
                    }
                    std::thread::sleep(interval);
                }
            })
        };
        Self {
            inner,
            stop,
            thread: Some(thread),
        }
    }

    /// Read the registry under the lock.
    ///
    /// **A poisoned lock is taken, not unwrapped.** A panic in one reader must not become the
    /// supervisor's death: §5.7 requires a supervisor with at least one non-terminal node to keep
    /// running, and it cannot do that from inside a `PoisonError` unwrap — the processes it is
    /// holding would outlive it, untracked, which is the exact outcome §7.2 spends a page
    /// preventing. This is `watch.rs`'s rule (*the observer must never end the thing it observes*)
    /// one layer up, and the same idiom `journal.rs` and `bin/marion.rs` already use.
    ///
    /// The state behind the lock survives it because the only thing done under it is
    /// [`Registry::poll`], and `Replay::extend` is total and infallible by construction — there is
    /// no half-applied record for a panic to leave behind.
    pub fn read<T>(&self, f: impl FnOnce(&Registry) -> T) -> T {
        f(&lock(&self.inner))
    }

    /// Fold bytes that are already on disk now, without waiting for the follower's next tick.
    ///
    /// A lifecycle handler cannot append an intent or confirmation and then answer from a stale
    /// tree: that would let a second quit race the poll interval and repeat an irreversible act.
    /// The background follower remains the ordinary path; decision points use this synchronous
    /// path because ordering, not elapsed time, is their contract.
    pub fn refresh(&self) -> usize {
        lock(&self.inner).poll()
    }

    /// Stop following and hand back the final reading, after one last poll.
    pub fn stop(mut self) -> Registry {
        self.halt();
        self.read(|r| r.clone())
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for LiveRegistry {
    /// A dropped handle must not leave a thread polling a file forever. `halt` is idempotent, so
    /// this is a no-op after [`LiveRegistry::stop`].
    fn drop(&mut self) {
        self.halt();
    }
}

fn lock(inner: &Mutex<Registry>) -> std::sync::MutexGuard<'_, Registry> {
    inner.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::contract::{AgentId, ExitStatus, ProcessExit};
    use marion_core::harness::Harness;
    use marion_core::journal::{
        Exited, JournalRecord, RecordKind, SpawnIntent, Spawned, WriterId, encode,
    };
    use marion_testsupport::scratch;
    use std::io::Write;
    use std::path::Path;

    fn line(writer: &str, seq: u64, kind: RecordKind) -> Vec<u8> {
        encode(&JournalRecord {
            writer: WriterId(writer.into()),
            seq,
            ts: marion_core::encoding::SystemTime::from_unix_millis(1_785_625_628_619),
            mono_ns: seq,
            provenance: marion_core::ir::Provenance::marion(),
            src_seq: None,
            kind,
        })
        .expect("a record encodes")
    }

    fn id(s: &str) -> AgentId {
        AgentId(s.into())
    }

    fn intent(agent: &str, parent: Option<&str>) -> RecordKind {
        RecordKind::SpawnIntent(SpawnIntent {
            agent_id: id(agent),
            parent_id: parent.map(id),
            agent_type: "codex-impl".into(),
            harness: Harness::Codex,
            depth: u32::from(parent.is_some()),
            task_id: None,
        })
    }

    fn spawned(agent: &str) -> RecordKind {
        RecordKind::Spawned(Spawned {
            agent_id: id(agent),
            harness_version: "0.9.0".into(),
            model: None,
            pid: Some(7),
        })
    }

    fn exited(agent: &str) -> RecordKind {
        RecordKind::Exited(Exited {
            agent_id: id(agent),
            status: ExitStatus::Ok,
            exit: ProcessExit {
                code: Some(0),
                signal: None,
                description: "clean exit".into(),
            },
        })
    }

    fn append(path: &Path, bytes: &[u8]) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(bytes).unwrap();
    }

    /// A journal with a root, a child and both terminals — written with the ordinals a real writer
    /// would produce, unless `skip` names one to leave out.
    fn journal(path: &Path, skip: Option<u64>) {
        let kinds = [
            intent("root", None),
            spawned("root"),
            intent("child", Some("root")),
            spawned("child"),
            exited("child"),
            exited("root"),
        ];
        for (seq, kind) in kinds.into_iter().enumerate() {
            let seq = seq as u64;
            if skip == Some(seq) {
                continue;
            }
            append(path, &line("w", seq, kind));
        }
    }

    /// **§7.2's marking is the boot's, not the poll's** — the one wiring claim this module makes.
    ///
    /// A node the journal already held when the supervisor started is a node whose fate this
    /// supervisor has no record of deciding, so it is `Orphaned`. A node that *appears while this
    /// supervisor is following* is its contemporary and must stay `Live` however long it runs;
    /// marking it would assert marion lost a node it is watching arrive.
    #[test]
    fn boot_marks_a_live_node_orphaned_and_a_node_that_arrives_afterwards_is_left_alone() {
        let dir = scratch("registry-restart-marking");
        let path = dir.join("journal.jsonl");
        append(&path, &line("w", 0, intent("before", None)));
        append(&path, &line("w", 1, spawned("before")));

        let mut r = Registry::boot_path(&path).expect("a readable journal boots");
        assert_eq!(
            r.restart_marks(),
            [crate::restart::Marked {
                agent_id: id("before"),
                marking: crate::restart::Marking::Orphaned,
            }],
        );
        assert_eq!(
            r.tree().get(&id("before")).unwrap().reap_state,
            marion_core::node::ReapState::Orphaned,
        );

        append(&path, &line("w", 2, intent("after", None)));
        append(&path, &line("w", 3, spawned("after")));
        assert_eq!(r.poll(), 2, "the two new records were folded in");
        assert_eq!(
            r.tree().get(&id("after")).unwrap().reap_state,
            marion_core::node::ReapState::Live,
            "a node this supervisor watched arrive is not one it lost",
        );
        assert_eq!(
            r.restart_marks().len(),
            1,
            "the boot's verdict list does not grow as the journal does",
        );
        assert_eq!(
            r.tree().get(&id("before")).unwrap().reap_state,
            marion_core::node::ReapState::Orphaned,
            "and polling does not undo the marking",
        );
    }

    /// A journal whose nodes all reached a recorded fate leaves the restart pass with nothing to
    /// say — the negative control for the test above, so "everything is orphaned" cannot pass it.
    #[test]
    fn booting_over_a_fully_resolved_journal_marks_nothing() {
        let dir = scratch("registry-restart-clean");
        let path = dir.join("journal.jsonl");
        journal(&path, None);
        let r = Registry::boot_path(&path).expect("a readable journal boots");
        assert!(
            r.restart_marks().is_empty(),
            "both nodes exited on the record"
        );
        for n in r.tree().nodes() {
            assert_eq!(
                n.reap_state,
                marion_core::node::ReapState::Live,
                "{} exited; it was never lost",
                n.agent_id.0
            );
        }
    }

    /// **Boot from replay is the registry's start**, and it can say what it read.
    #[test]
    fn a_registry_boots_from_an_existing_journal_and_states_its_own_read_point() {
        let dir = scratch("registry-boot");
        let path = dir.join("journal.jsonl");
        journal(&path, None);

        let r = Registry::boot_path(&path).expect("a readable journal boots");
        assert_eq!(
            r.tree()
                .nodes()
                .iter()
                .map(|n| n.agent_id.0.as_str())
                .collect::<Vec<_>>(),
            ["root", "child"],
        );
        assert_eq!(
            r.tree().get(&id("child")).unwrap().parent_id(),
            Some(&id("root"))
        );
        assert_eq!(r.boot_point().records, 6);
        assert_eq!(r.boot_point().src_seq, None);
        assert!(r.booted_complete());
        assert_eq!(r.status(), &Status::Following);
        // §7.3.3's seam is a *value*, not an implication: it is the same before any poll…
        assert_eq!(r.read_point(), r.boot_point().clone());
    }

    /// **NC — a gap at boot is visible, and the tree alone cannot show it.**
    ///
    /// The two journals differ by one lost record and produce the **same tree**: `Spawned` for a
    /// node that also has an `Exited` moves nothing replay reports structurally. So if the gap were
    /// not carried separately, a registry booted over a hole would be indistinguishable from one
    /// booted clean — which is this repo's documented failure class, a partial tree presented as
    /// complete. The assertion is therefore stated *both* ways: the trees are equal, and the
    /// registries are not.
    #[test]
    fn a_registry_booted_over_a_lost_record_is_distinguishable_from_one_booted_clean() {
        let dir = scratch("registry-gap");
        let clean_path = dir.join("clean.jsonl");
        let holed_path = dir.join("holed.jsonl");
        journal(&clean_path, None);
        journal(&holed_path, Some(3));

        let clean = Registry::boot_path(&clean_path).unwrap();
        let holed = Registry::boot_path(&holed_path).unwrap();

        assert_eq!(
            clean
                .tree()
                .nodes()
                .iter()
                .map(|n| n.agent_id.clone())
                .collect::<Vec<_>>(),
            holed
                .tree()
                .nodes()
                .iter()
                .map(|n| n.agent_id.clone())
                .collect::<Vec<_>>(),
            "same nodes and same edges: the tree is exactly where the loss does not show"
        );

        assert!(clean.booted_complete());
        assert!(clean.boot_gaps().is_empty());
        assert!(
            !holed.booted_complete(),
            "a registry over a hole must not claim a complete reading"
        );
        assert_eq!(
            holed.boot_gaps(),
            [SeqGap {
                writer: WriterId("w".into()),
                expected: 3,
                found: 4,
            }],
            "§4.2's Ordinal loss, named: which writer, what was expected, what arrived"
        );
    }

    /// A complete line that is not a record is corruption on an append-only file. The registry
    /// serves the intact prefix, **says it stopped**, and never advances past it — including over
    /// records appended afterwards, which is the half that would otherwise look like recovery.
    #[test]
    fn a_registry_booted_over_a_corrupt_line_serves_the_prefix_and_says_it_stopped() {
        let dir = scratch("registry-corrupt");
        let path = dir.join("journal.jsonl");
        append(&path, &line("w", 0, intent("root", None)));
        append(&path, b"this is not a journal record at all\n");
        append(&path, &line("w", 1, intent("child", Some("root"))));

        let mut r = Registry::boot_path(&path).expect("a partially readable journal still boots");
        assert_eq!(r.tree().nodes().len(), 1, "the intact prefix, and no more");
        assert!(!r.booted_complete());
        let Status::Stopped { reason } = r.status() else {
            panic!("{:?}", r.status())
        };
        assert!(reason.contains("not a journal record"), "{reason}");

        append(&path, &line("w", 2, exited("root")));
        assert_eq!(r.poll(), 0, "a stopped registry reads nothing further");
        assert_eq!(r.tree().nodes().len(), 1);
    }

    /// A project that has never run has no journal, and that is an empty tree rather than a fault.
    /// An **unreadable** one is the opposite: reporting an empty tree for a permission or hardware
    /// fault would claim marion has no nodes, which is the claim it least may make.
    #[test]
    fn a_missing_journal_boots_empty_and_an_unreadable_one_refuses_to_boot() {
        let dir = scratch("registry-missing");
        let r =
            Registry::boot_path(&dir.join("never-ran.jsonl")).expect("never run is not a fault");
        assert!(r.tree().nodes().is_empty());
        assert_eq!(r.boot_point().records, 0);
        assert!(r.booted_complete(), "nothing read, nothing missing");

        let as_dir = dir.join("journal-is-a-directory");
        std::fs::create_dir_all(&as_dir).unwrap();
        assert!(
            Registry::boot_path(&as_dir).is_err(),
            "a journal marion cannot read is not a journal with no records"
        );
    }

    /// **Staying current.** Records appended after boot — by a *second* writer, which is the whole
    /// reason the registry tails the file rather than being updated where marion emits (the bridge
    /// that writes a child's records is a different process) — reach the tree.
    #[test]
    fn a_booted_registry_picks_up_records_a_second_writer_appends_afterwards() {
        let dir = scratch("registry-follow");
        let path = dir.join("journal.jsonl");
        append(&path, &line("run", 0, intent("root", None)));
        let mut r = Registry::boot_path(&path).unwrap();
        assert_eq!(r.tree().nodes().len(), 1);
        assert_eq!(r.poll(), 0, "nothing new is not news");

        // The bridge process, with its own writer identity and its own ordinal from 0.
        append(&path, &line("bridge", 0, intent("child", Some("root"))));
        append(&path, &line("bridge", 1, spawned("child")));
        assert_eq!(r.poll(), 2);
        let child = r
            .tree()
            .get(&id("child"))
            .expect("the child is in the tree");
        assert_eq!(child.parent_id(), Some(&id("root")));
        assert!(child.spawn_confirmed);
        assert!(r.tree().gaps.is_empty(), "sequences are per-writer");

        append(&path, &line("bridge", 2, exited("child")));
        assert_eq!(r.poll(), 1);
        assert_eq!(
            r.tree().get(&id("child")).unwrap().state,
            marion_core::node::NodeState::Exited(ExitStatus::Ok)
        );
        // …and the read point moved with it, while the boot point did not.
        assert_eq!(r.boot_point().records, 1);
        assert_eq!(r.read_point().records, 4);
    }

    /// A half-written record is the normal case — the writer is another process appending — and it
    /// must be read **once**, when it is finished, never as garbage and never twice.
    #[test]
    fn a_record_being_written_while_the_registry_polls_is_folded_in_once_when_complete() {
        let dir = scratch("registry-torn");
        let path = dir.join("journal.jsonl");
        append(&path, &line("w", 0, intent("root", None)));
        let mut r = Registry::boot_path(&path).unwrap();

        let whole = line("w", 1, intent("child", Some("root")));
        let (head, tail) = whole.split_at(whole.len() / 2);
        append(&path, head);
        assert_eq!(r.poll(), 0, "half a record is not a node");
        assert_eq!(r.tree().nodes().len(), 1);
        assert_eq!(
            r.status(),
            &Status::Following,
            "a torn tail is expected, not a fault"
        );

        append(&path, tail);
        assert_eq!(r.poll(), 1);
        assert_eq!(r.tree().nodes().len(), 2);
        assert_eq!(r.poll(), 0, "and not a second time");
    }

    /// **NC — the four readings a follower must keep apart.**
    ///
    /// *nothing new*, *something new*, *this poll could not read*, and *stopped for good*. Collapse
    /// the third into the first and a journal that vanished looks exactly like a journal with
    /// nothing to say; collapse the fourth and a registry that has stopped following looks like a
    /// tree that stopped changing. Both collapses present a stale tree as a current one, which is
    /// the thing a registry exists not to do.
    #[test]
    fn a_registry_that_stopped_updating_is_distinguishable_from_a_journal_that_went_quiet() {
        let dir = scratch("registry-liveness");

        // (1) quiet: following, polling, nothing new.
        let quiet_path = dir.join("quiet.jsonl");
        journal(&quiet_path, None);
        let mut quiet = Registry::boot_path(&quiet_path).unwrap();
        let before = quiet.read_point();
        for _ in 0..3 {
            assert_eq!(quiet.poll(), 0);
        }
        assert_eq!(quiet.status(), &Status::Following);
        assert_eq!(quiet.polls(), 3);
        assert_eq!(quiet.read_point(), before, "quiet is not stale");

        // (2) advancing: the same status, a different reading.
        append(&quiet_path, &line("w", 6, intent("second-root", None)));
        assert_eq!(quiet.poll(), 1);
        assert_eq!(quiet.status(), &Status::Following);
        assert_ne!(quiet.read_point(), before);

        // (3) unreadable: the journal it was following is gone. **Not** the same as quiet.
        let gone_path = dir.join("gone.jsonl");
        journal(&gone_path, None);
        let mut gone = Registry::boot_path(&gone_path).unwrap();
        std::fs::remove_file(&gone_path).unwrap();
        assert_eq!(gone.poll(), 0);
        let Status::Unreadable { reason } = gone.status() else {
            panic!(
                "a journal that vanished must not read as one with nothing new: {:?}",
                gone.status()
            )
        };
        assert!(!reason.is_empty());
        // Transient by construction: the next poll retries, and a journal that comes back is
        // followed again rather than being written off. **The status must clear on the read that
        // succeeds, not on the read that finds something** — otherwise a registry that is provably
        // current keeps announcing it might be stale.
        journal(&gone_path, None);
        assert_eq!(gone.poll(), 0, "the same bytes are not new bytes");
        assert_eq!(gone.status(), &Status::Following);
        append(&gone_path, &line("w", 6, intent("second-root", None)));
        assert_eq!(gone.poll(), 1);
        assert!(gone.tree().get(&id("second-root")).is_some());

        // (4) stopped: corruption. Distinguishable from all three above, and permanent.
        let dead_path = dir.join("dead.jsonl");
        journal(&dead_path, None);
        let mut dead = Registry::boot_path(&dead_path).unwrap();
        append(&dead_path, b"not a record\n");
        append(&dead_path, &line("w", 6, intent("second-root", None)));
        assert_eq!(dead.poll(), 0);
        assert!(matches!(dead.status(), Status::Stopped { .. }));
        assert_eq!(dead.poll(), 0);
        assert!(
            dead.tree().get(&id("second-root")).is_none(),
            "a stopped registry must not silently resume"
        );

        // And no two of the four statuses are the same value, so nothing downstream can conflate
        // them by accident.
        let all = [
            quiet.status().clone(),
            gone_status(),
            dead.status().clone(),
            Status::Following,
        ];
        for i in 0..all.len() {
            for k in i + 1..all.len() {
                if i == 0 && k == 3 {
                    continue; // quiet *is* Following; that is the point of (1).
                }
                assert_ne!(all[i], all[k], "statuses {i} and {k} are indistinguishable");
            }
        }
    }

    fn gone_status() -> Status {
        Status::Unreadable {
            reason: "the journal is gone".into(),
        }
    }

    /// An append-only file never gets shorter. If one does it is not the file the registry booted
    /// against, and an **authority** may not keep serving a tree from a file it no longer
    /// recognises — where a viewer (`watch.rs`) may simply show less, this must stop and say so.
    #[test]
    fn a_journal_that_shrank_stops_the_registry_rather_than_being_served_as_a_tree() {
        let dir = scratch("registry-shrink");
        let path = dir.join("journal.jsonl");
        journal(&path, None);
        let mut r = Registry::boot_path(&path).unwrap();
        std::fs::write(&path, b"").unwrap();
        assert_eq!(r.poll(), 0);
        let Status::Stopped { reason } = r.status() else {
            panic!("{:?}", r.status())
        };
        assert!(reason.contains("shorter"), "{reason}");
    }

    /// The follower keeps the registry current **without its reader polling** — which is what makes
    /// it a running registry rather than a function someone must remember to call.
    #[test]
    fn a_followed_registry_becomes_current_without_its_reader_asking() {
        let dir = scratch("registry-live");
        let path = dir.join("journal.jsonl");
        append(&path, &line("w", 0, intent("root", None)));
        let live = LiveRegistry::follow(
            Registry::boot_path(&path).unwrap(),
            std::time::Duration::from_millis(5),
        );
        assert_eq!(live.read(|r| r.tree().nodes().len()), 1);

        append(&path, &line("w", 1, intent("child", Some("root"))));
        assert!(
            until(|| live.read(|r| r.tree().get(&id("child")).is_some())),
            "the follower never picked the child up"
        );
        let polls = live.read(|r| r.polls());
        assert!(polls > 0, "the follower polled");

        // Stopping joins, and the final poll happens **after** the stop is decided — the same rule
        // `bin/marion.rs`'s `follow_journal` is written to, so the last records written before a
        // shutdown are not lost.
        append(&path, &line("w", 2, exited("root")));
        let final_tree = live.stop();
        assert!(
            final_tree
                .tree()
                .get(&id("root"))
                .unwrap()
                .state
                .is_exited(),
            "a follower that returns before its last poll ends the reading on a lie"
        );
    }

    /// **NC — a panicking reader must not end the registry.**
    ///
    /// `watch.rs`'s rule one level up: the observer may never end the thing it observes. A poisoned
    /// lock is taken anyway, because refusing it would turn one thread's panic into the
    /// supervisor's death — and §5.7 says a supervisor with live nodes MUST keep running, which it
    /// cannot do from inside a `PoisonError` unwrap.
    #[test]
    fn a_poisoned_registry_is_still_readable_rather_than_taking_the_supervisor_down_with_it() {
        let dir = scratch("registry-poison");
        let path = dir.join("journal.jsonl");
        journal(&path, None);
        let live = LiveRegistry::follow(
            Registry::boot_path(&path).unwrap(),
            std::time::Duration::from_millis(5),
        );

        let hushed = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            live.read(|_| panic!("a reader blew up while holding the lock"));
        }));
        std::panic::set_hook(hushed);
        assert!(panicked.is_err(), "the panic really happened");

        assert_eq!(
            live.read(|r| r.tree().nodes().len()),
            2,
            "the tree is still served after the lock was poisoned"
        );
        // …and the follower thread, which takes the same lock, is still following.
        append(&path, &line("w", 6, intent("second-root", None)));
        assert!(until(
            || live.read(|r| r.tree().get(&id("second-root")).is_some())
        ));
    }

    /// Wait for a condition, checking often, up to a bound generous enough that a loaded machine
    /// does not decide the answer. Returns whether it became true — never a bare sleep, so a fast
    /// machine does not pay for a slow one's headroom.
    fn until(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        cond()
    }
}
