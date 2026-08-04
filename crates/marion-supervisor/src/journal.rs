//! The journal **writer** (design §4.3) — `marion-core` owns the records and the replay, this owns
//! the file.
//!
//! ```text
//! <state>/<project-hash>/journal.jsonl      # append-only registry journal
//! ```
//!
//! # Durability: group commit, and only barriers pay for an fsync
//!
//! §4.3: *"Append without fsync; fsync on a ~50 ms timer **and** unconditionally at each state
//! transition that must survive a crash — `Spawned`, `Exited`, `ReapedIdle`."* §12 records that
//! per-record fsync was **replaced** by exactly this, so it is not reintroduced here: a barrier
//! record ([`marion_core::journal::RecordKind::is_barrier`]) is fsynced before the call returns,
//! and everything else is fsynced by the next barrier or by the first append after
//! [`GROUP_COMMIT_INTERVAL`] has elapsed, whichever comes first. Losing a non-barrier record costs
//! a slightly stale replay; losing a barrier costs an untracked live process, and §4.3 says only
//! the latter is worth the syscall.
//!
//! **The ordering inside a barrier is load-bearing** and the callers are written to it: *"append
//! the record, fsync it, **then** perform and announce the transition."* For an act marion cannot
//! undo — starting a process — that ordering applies to the **intent**: fsync the intent, do the
//! act, then append and fsync the confirmation. [`Journal::append`] returning `Ok` is therefore the
//! signal that it is safe to proceed, which is why it returns before the caller acts rather than
//! after.
//!
//! # Concurrency: `O_APPEND`, not a lock
//!
//! `marion run` and each `marion-supervisor mcp` bridge are **separate processes** that both cause
//! lifecycle events, so the file has concurrent writers by construction. marion serialises them
//! with `O_APPEND` atomicity rather than a lock:
//!
//! * POSIX requires that for a file opened `O_APPEND`, the seek-to-end and the write happen
//!   atomically with respect to other writers. Every record is written with **one** `write(2)`, so
//!   two writers interleave at record granularity and never inside a record.
//! * **The size limit that holds under is stated and enforced**: `marion_core::journal::MAX_RECORD_BYTES`
//!   (16 KiB), checked at encode time, so an over-large record is refused rather than written and
//!   torn. Records here carry ids, versions and marion's own descriptions; nothing approaches it.
//! * A short write is nonetheless handled rather than assumed away (a signal can cut one): the
//!   writer immediately appends a lone `\n` to **close the torn line**, so a concurrent writer's
//!   next record can never be glued onto the fragment, and reports [`JournalError::TornWrite`].
//!   Replay then discards the fragment as an unparsable line.
//!
//! A lock was rejected on two grounds. The workspace's dependency set is deliberately
//! serde/serde_json/thiserror, and `flock` is not in `std` — it would mean either a new dependency
//! or a hand-rolled `extern "C"` declaration for a guarantee `O_APPEND` already gives. And a lock
//! held across an fsync serialises two processes on the slowest syscall in the path, which is
//! precisely what group commit exists to avoid. Ordering is not lost by dropping the lock: §4.3
//! makes the journal's total order the **file's** order, and the kernel assigns it.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marion_core::encoding::SystemTime;
use marion_core::ir::Provenance;
use marion_core::journal::{EncodeError, JournalRecord, RecordKind, WriterId, encode};
use marion_core::paths::ProjectDir;
use marion_core::registry::{Replay, replay};

/// §4.3's *"~50 ms timer"*.
pub const GROUP_COMMIT_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("journal io: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Encode(#[from] EncodeError),
    #[error(
        "a journal record was written short ({written} of {len} bytes); the torn line has been \
         closed with a newline so no concurrent writer's record can be glued onto it, and replay \
         will discard it"
    )]
    TornWrite { written: usize, len: usize },
}

/// An open `journal.jsonl`, with this process's writer identity and ordinal.
pub struct Journal {
    path: PathBuf,
    file: File,
    writer: WriterId,
    seq: u64,
    /// Bytes are in the page cache and not yet on disk.
    dirty: bool,
    last_sync: Instant,
    /// §4.2's `mono_ns` origin: monotonic since this writer started.
    start: Instant,
}

impl Journal {
    /// Open (creating) the project's journal.
    ///
    /// `writer` names this process. It must be unique per process — the per-writer ordinal is what
    /// makes loss detectable (§4.2's `Ordinal` form), and two processes sharing an identity would
    /// read as one writer emitting an interleaved, gap-ridden sequence.
    pub fn open(project: &ProjectDir, writer: WriterId) -> Result<Self, JournalError> {
        std::fs::create_dir_all(project.path())?;
        Self::open_path(&project.journal(), writer)
    }

    /// The same, naming the file directly. Exists for tests and for a replay tool pointed at a
    /// journal outside a state directory.
    pub fn open_path(path: &Path, writer: WriterId) -> Result<Self, JournalError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            writer,
            seq: 0,
            dirty: false,
            last_sync: Instant::now(),
            start: Instant::now(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn writer(&self) -> &WriterId {
        &self.writer
    }

    /// Append one record, applying §4.3's commit policy.
    ///
    /// Returns once the record is **durable if it is a barrier**, which is the guarantee callers
    /// order their side effects against.
    pub fn append(&mut self, kind: RecordKind) -> Result<JournalRecord, JournalError> {
        let barrier = kind.is_barrier();
        let record = JournalRecord {
            writer: self.writer.clone(),
            seq: self.seq,
            ts: SystemTime(std::time::SystemTime::now()),
            mono_ns: self.start.elapsed().as_nanos() as u64,
            // Every journal record is marion's own observation of its own decision: §4.1's
            // `Source::Marion`, authoritative because no other system knows marion made it.
            provenance: Provenance::marion(),
            // §4.2: marion is the source, and the source has no upstream ordering to report. The
            // record's own ordinal is `seq`. Writing something here would be inventing evidence.
            src_seq: None,
            kind,
        };
        let line = encode(&record)?;
        // **One** `write(2)`, never `write_all`: a loop that re-issues the remainder is exactly the
        // interleaving `O_APPEND` atomicity is being relied on to prevent.
        let written = self.file.write(&line)?;
        self.seq += 1;
        self.dirty = true;
        if written != line.len() {
            // Close the fragment so the next writer's record cannot continue this line.
            let _ = self.file.write(b"\n");
            let _ = self.file.sync_data();
            self.dirty = false;
            self.last_sync = Instant::now();
            return Err(JournalError::TornWrite {
                written,
                len: line.len(),
            });
        }
        if barrier || self.last_sync.elapsed() >= GROUP_COMMIT_INTERVAL {
            self.sync()?;
        }
        Ok(record)
    }

    /// Best-effort append for a call site that must not change its behaviour on a journal fault.
    ///
    /// The journal is being introduced under working code paths, and a run that succeeds today
    /// must still succeed. The failure is **reported on stderr rather than swallowed** — a silent
    /// journal is the failure mode §12 keeps recording — and promoting it to fatal is the right
    /// change on the day the registry actually reads this file to make decisions, not before.
    pub fn record(&mut self, kind: RecordKind) {
        if let Err(e) = self.append(kind) {
            eprintln!("marion: journal write failed: {e}");
        }
    }

    /// fsync if anything is pending. `sync_data` rather than `sync_all`: the file's length and
    /// contents are the durability claim, and its mtime is not.
    pub fn sync(&mut self) -> Result<(), JournalError> {
        if self.dirty {
            self.file.sync_data()?;
            self.dirty = false;
        }
        self.last_sync = Instant::now();
        Ok(())
    }

    /// §4.3's timer half, for a caller with an idle loop. Appending is what normally triggers it.
    pub fn tick(&mut self) {
        if self.dirty && self.last_sync.elapsed() >= GROUP_COMMIT_INTERVAL {
            let _ = self.sync();
        }
    }
}

impl Drop for Journal {
    /// The last group's commit. Without it a clean shutdown could lose non-barrier records that a
    /// crash would have lost anyway — a difference the reader would have no way to explain.
    fn drop(&mut self) {
        let _ = self.sync();
    }
}

/// Read a project's journal back into the tree it records.
///
/// **A missing journal is an empty tree, not an error**: a project that has never run has no file,
/// and that is the same information as a file with no records. An unreadable one *is* an error —
/// that is a permission or hardware fault, and reporting an empty tree for it would silently claim
/// marion has no nodes.
pub fn read(project: &ProjectDir) -> Result<Replay, JournalError> {
    read_path(&project.journal())
}

pub fn read_path(path: &Path) -> Result<Replay, JournalError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(replay(&bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Replay::default()),
        Err(e) => Err(e.into()),
    }
}

/// The journals this process has open, one per file, keyed by path.
///
/// **One `Journal` per file per process, held open for the process's life**, because the per-writer
/// ordinal is only gapless if one object owns it: reopening the file for each record would restart
/// `seq` at 0 and replay would read the run as a writer emitting duplicate ordinals, which is
/// exactly the loss signal [`marion_core::registry::SeqGap`] exists to make meaningful. A `Vec`
/// rather than a map because a supervisor process serves one project — `marion run` opens exactly
/// one, a bridge exactly one — and only the test binary, which drives many temp projects in one
/// process, ever holds more than a single entry.
static OPEN: std::sync::Mutex<Vec<(PathBuf, Journal)>> = std::sync::Mutex::new(Vec::new());

/// This process's writer identity, minted once. See [`WriterId`]: two writers sharing an identity
/// read as one writer emitting an interleaved, gap-ridden sequence.
fn this_writer() -> WriterId {
    static ID: std::sync::OnceLock<WriterId> = std::sync::OnceLock::new();
    ID.get_or_init(writer_id).clone()
}

/// **Record one lifecycle transition, and never fail the run over it.**
///
/// This is the failure policy, stated once and applied identically to a root (`root::prepare`,
/// `root::launch`) and to a child (`run::run_spawn`), because a policy that differed between them
/// would make the journal's meaning depend on which node it is about.
///
/// The argument, from the spec. §9's M2 criteria exist to prevent an **untracked live process** —
/// a node marion started and has no record of. That is what makes a lost record serious, and it is
/// why §4.3 buys an fsync for the barrier records and not for the rest. But *today* nothing reads
/// this file to make a decision: there is no registry, no descendant gate and no reap recovery on
/// top of it yet, so a journal fault costs a stale replay and nothing else. Against that, promoting
/// it to fatal would kill a working run — a real child in a real worktree, mid-edit — over a full
/// disk, and would introduce a brand-new way for `marion run` to fail that no existing behaviour
/// has. So: **the run continues, and the fault is loud on stderr rather than swallowed**, which is
/// the failure mode §12 keeps recording. The day the registry actually reads this file to decide
/// whether a process may be spawned, this becomes fatal — and that is a change to this one
/// function, not to any call site.
pub fn record(project: &ProjectDir, kind: RecordKind) {
    let path = project.journal();
    let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    if !open.iter().any(|(p, _)| *p == path) {
        match Journal::open(project, this_writer()) {
            Ok(j) => open.push((path.clone(), j)),
            Err(e) => {
                eprintln!("marion: cannot open the journal {}: {e}", path.display());
                return;
            }
        }
    }
    if let Some((_, j)) = open.iter_mut().find(|(p, _)| *p == path) {
        j.record(kind);
    }
}

/// This process's writer identity: pid plus a UUIDv7, so two runs of one pid (recycled after a
/// reboot, or in a container) never collide.
pub fn writer_id() -> WriterId {
    let unique = match crate::run::entropy() {
        Ok(e) => marion_core::ids::uuid_v7(crate::run::unix_millis(), e),
        // Entropy is unavailable only in a state marion cannot run in anyway; the pid alone still
        // separates this writer from a concurrent bridge, which is what the ordinal needs.
        Err(_) => format!("no-entropy-{}", crate::run::unix_millis()),
    };
    WriterId(format!("{}-{unique}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::contract::{AgentId, ExitStatus, ProcessExit, TaskId};
    use marion_core::harness::Harness;
    use marion_core::journal::{Exited, SpawnIntent, Spawned, StateChanged};
    use marion_core::node::NodeState;

    fn temp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "marion-journal-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn intent(id: &str, parent: Option<&str>) -> RecordKind {
        RecordKind::SpawnIntent(SpawnIntent {
            agent_id: AgentId(id.into()),
            parent_id: parent.map(|p| AgentId(p.into())),
            agent_type: "claude".into(),
            harness: Harness::ClaudeCode,
            depth: u32::from(parent.is_some()),
            task_id: parent.map(|_| TaskId("t-1".into())),
        })
    }

    #[test]
    fn a_written_journal_replays_to_the_tree_that_was_written() {
        let dir = temp("round-trip");
        let path = dir.join("journal.jsonl");
        {
            let mut j = Journal::open_path(&path, WriterId("w".into())).unwrap();
            j.append(intent("root", None)).unwrap();
            j.append(RecordKind::Spawned(Spawned {
                agent_id: AgentId("root".into()),
                harness_version: "2.1.220".into(),
                model: None,
                pid: Some(1),
            }))
            .unwrap();
            j.append(intent("child", Some("root"))).unwrap();
            j.append(RecordKind::StateChanged(StateChanged {
                agent_id: AgentId("child".into()),
                state: NodeState::Running,
            }))
            .unwrap();
            j.append(RecordKind::Exited(Exited {
                agent_id: AgentId("child".into()),
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "clean exit".into(),
                },
            }))
            .unwrap();
        }
        let r = read_path(&path).unwrap();
        assert_eq!(r.records, 5);
        assert_eq!(r.truncation, None);
        assert!(r.gaps.is_empty(), "one writer, gapless by construction");
        assert_eq!(r.nodes().len(), 2);
        let child = r.get(&AgentId("child".into())).unwrap();
        assert_eq!(child.parent_id(), Some(&AgentId("root".into())));
        assert_eq!(child.state, NodeState::Exited(ExitStatus::Ok));
        assert_eq!(child.task_id(), Some(&TaskId("t-1".into())));
    }

    #[test]
    fn a_second_open_appends_rather_than_truncating() {
        let dir = temp("append");
        let path = dir.join("journal.jsonl");
        Journal::open_path(&path, WriterId("a".into()))
            .unwrap()
            .append(intent("root", None))
            .unwrap();
        Journal::open_path(&path, WriterId("b".into()))
            .unwrap()
            .append(intent("child", Some("root")))
            .unwrap();
        let r = read_path(&path).unwrap();
        assert_eq!(r.records, 2, "the second open must not have truncated");
        assert_eq!(r.nodes().len(), 2);
    }

    #[test]
    fn a_barrier_record_is_durable_before_append_returns() {
        // The property callers order their side effects against. Observable here only as "the
        // writer considers nothing pending"; the fsync itself is the kernel's business.
        let dir = temp("barrier");
        let path = dir.join("journal.jsonl");
        let mut j = Journal::open_path(&path, WriterId("w".into())).unwrap();
        j.append(intent("root", None)).unwrap();
        assert!(!j.dirty, "a barrier record fsyncs before append returns");

        j.append(RecordKind::StateChanged(StateChanged {
            agent_id: AgentId("root".into()),
            state: NodeState::Running,
        }))
        .unwrap();
        assert!(
            j.dirty,
            "a non-barrier record rides the ~50 ms timer — reintroducing fsync-per-record is \
             exactly what §12 records as replaced"
        );
        // …and is still readable immediately, because the bytes are in the file either way.
        assert_eq!(read_path(&path).unwrap().records, 2);
    }

    #[test]
    fn the_group_commit_timer_flushes_a_pending_record() {
        let dir = temp("timer");
        let path = dir.join("journal.jsonl");
        let mut j = Journal::open_path(&path, WriterId("w".into())).unwrap();
        j.append(RecordKind::StateChanged(StateChanged {
            agent_id: AgentId("root".into()),
            state: NodeState::Running,
        }))
        .unwrap();
        assert!(j.dirty);
        std::thread::sleep(GROUP_COMMIT_INTERVAL + Duration::from_millis(10));
        j.tick();
        assert!(
            !j.dirty,
            "the ~50 ms timer is the other half of §4.3's rule"
        );
    }

    #[test]
    fn a_missing_journal_is_an_empty_tree_not_an_error() {
        let dir = temp("missing");
        let r = read_path(&dir.join("nope.jsonl")).unwrap();
        assert_eq!(r.records, 0);
        assert!(r.nodes().is_empty());
    }

    #[test]
    fn writer_ids_are_distinct_within_a_process() {
        // Two bridges under one supervisor would otherwise share an identity and read as one
        // writer emitting a gap-ridden sequence.
        assert_ne!(writer_id(), writer_id());
    }
}
