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
//! One project's journal has concurrent writers by construction, and **§11 item 28 changed who they
//! are without reducing how many**. Until steps 5 and 6 they were separate *processes*: `marion run`
//! wrote the root's records and each `marion-supervisor mcp` bridge wrote its own children's. Now
//! the supervisor owns every node, so the writers are its per-node threads — one per running node,
//! plus its own — and a restarted supervisor appending after a crashed one is still a second
//! process against the same file. marion serialises all of them with `O_APPEND` atomicity rather
//! than a lock, which covers both cases with one mechanism:
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
//! **The process case is not hypothetical after the move, and it is the reason nothing here
//! narrowed to an in-process lock.** A supervisor that dies leaves a journal a successor appends
//! to; §7.2's restart marking is written by that successor over records the dead one wrote. An
//! in-process mutex would be correct for the common case and silently wrong for exactly the case
//! recovery depends on.
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

use marion_core::contract::AgentId;
use marion_core::encoding::SystemTime;
use marion_core::ir::Provenance;
use marion_core::journal::{
    EncodeError, JournalRecord, PermissionDenied, RecordKind, Spawned, StateChanged, WriterId,
    encode,
};
use marion_core::node::NodeState;
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
    /// Open (creating) a journal file, naming the writer.
    ///
    /// **Not the door production uses.** A `Journal` owns `seq` and `mono_ns`'s origin, so a second
    /// one on the same file restarts both; [`append_at`] and [`record`] go through the process-wide
    /// [`OPEN`] pool for exactly that reason and this is what the pool calls. It stays public for
    /// tests that mean to open twice — `a_second_open_appends_rather_than_truncating` is
    /// about the *file* not being truncated — and for a replay tool pointed at a journal outside a
    /// state directory.
    ///
    /// `writer` names this process. It must be unique per process — the per-writer ordinal is what
    /// makes loss detectable (§4.2's `Ordinal` form), and two processes sharing an identity would
    /// read as one writer emitting an interleaved, gap-ridden sequence.
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

    /// Best-effort append for a call site whose record costs a **stale replay** if it is lost.
    ///
    /// The failure is reported on stderr rather than swallowed — a silent journal is the failure
    /// mode §12 keeps recording — and the run continues. See the free function [`record`] for which
    /// records that is right for and, more importantly, for the one it is **not**.
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
///
/// **`seq` is not the only field a second handle would ruin, and the other one is worse.**
/// `handler.rs` used to open its own `Journal` per `session/quit`, with a *fresh* [`writer_id`]
/// each time. That produced no [`marion_core::registry::SeqGap`] — replay seeds a writer's
/// expectation from the first ordinal it sees — so the damage was silent: a supervisor that is one
/// process read back as a crowd of one-record writers, and `mono_ns`, whose whole stated job (§4.2)
/// is to anchor a record against `pty.cast`, was a few microseconds from zero on every one of them.
/// [`append_at`] is that call path routed through here; it is the reason the function exists.
static OPEN: std::sync::Mutex<Vec<(PathBuf, Journal)>> = std::sync::Mutex::new(Vec::new());

/// This process's writer identity, minted once. See [`WriterId`]: two writers sharing an identity
/// read as one writer emitting an interleaved, gap-ridden sequence.
fn this_writer() -> WriterId {
    static ID: std::sync::OnceLock<WriterId> = std::sync::OnceLock::new();
    ID.get_or_init(writer_id).clone()
}

/// This process's handle on `path`, opened on first use and kept.
///
/// **The only way to reach a [`Journal`] from inside the supervisor**, and that is the point rather
/// than a convenience: `seq` counts from 0 per object and `mono_ns` from the object's own `start`,
/// so a second handle on one file — even in one process, even under [`this_writer`]'s single
/// identity — restarts both. See [`OPEN`].
fn entry<'a>(
    open: &'a mut Vec<(PathBuf, Journal)>,
    path: &Path,
) -> Result<&'a mut Journal, JournalError> {
    if let Some(i) = open.iter().position(|(p, _)| p == path) {
        return Ok(&mut open[i].1);
    }
    let journal = Journal::open_path(path, this_writer())?;
    open.push((path.to_path_buf(), journal));
    Ok(&mut open.last_mut().expect("just pushed").1)
}

/// Append one record to this process's handle on `path`, and **hand the failure back**.
///
/// [`record`]'s policy — report on stderr, never fail the run — is right for a lifecycle transition
/// nothing reads yet, and wrong for `session/quit`, which orders a kill against a *durable* intent
/// and has to be told when the append did not happen. So the difference between the two is the
/// failure policy and nothing else; both go through one handle, because the alternative is the
/// per-call open this function exists to remove.
pub fn append_at(path: &Path, kind: RecordKind) -> Result<JournalRecord, JournalError> {
    let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    entry(&mut open, path)?.append(kind)
}

/// [`record`]'s destination with [`append_at`]'s failure policy: **one project's journal, and the
/// caller is told when the record did not land.**
///
/// The door for a record a caller has to *act* on the loss of, which today is `Spawned` and only
/// `Spawned` — see [`record`] for why that record is not like the others. It exists as its own name
/// rather than as `append_at(&project.journal(), …)` at each call site so that the set of records
/// marion refuses to lose is greppable, and so that the two spawn paths cannot drift into spelling
/// the same decision differently.
pub fn append(project: &ProjectDir, kind: RecordKind) -> Result<JournalRecord, JournalError> {
    append_at(&project.journal(), kind)
}

/// **Record one lifecycle transition whose loss costs a stale replay, and never fail the run over
/// it.**
///
/// This is the failure policy, stated once and applied identically to a root (`root::prepare`,
/// `root::launch`) and to a child (`run::run_spawn`), because a policy that differed between them
/// would make the journal's meaning depend on which node it is about.
///
/// **The scope of "never fail the run" narrowed once, and the doc that used to sit here is the
/// reason it had to.** It argued that *"today nothing reads this file to make a decision"*, so a
/// lost record costs a stale replay and killing a live child over a full disk would be the worse
/// trade. That premise stopped being true: `marion_core::registry::replay` backs `tree/subscribe`,
/// `restart` recovers from it, `procid::audit` decides §9's *"no untracked live process"* from it,
/// residency and `session/quit` read it, and `session/quit`'s own intent already goes through the
/// fallible [`append_at`] for exactly that reason. A comment arguing for behaviour the code no
/// longer has is worse than none, so it is gone rather than softened.
///
/// What survives the narrowing is the trade itself, applied per record instead of wholesale. A
/// `StateChanged`, an `Exited`, a `PermissionDenied`, a `SpawnAborted` — losing any of these leaves
/// a node marion can still *name*: replay knows it exists and knows its pid, so `procid::audit` can
/// still resolve it and `restart` can still offer it. Stale, not invisible. Killing a real child
/// mid-edit over a full disk buys nothing against that, so those keep this policy and stay loud on
/// stderr.
///
/// **`Spawned` is the one record that is not like the others, and it does not come through here.**
/// It is the only record that carries a pid, so losing it does not make the tree stale — it makes
/// the process *unnameable*. `procid::audit`'s scope is `node.pid.is_some()`, so a live child whose
/// `Spawned` never landed is invisible to the audit, which is §11 item 30's untracked live process
/// exactly — the shape §9's criterion 3 exists to exclude. Its call sites therefore use [`append`],
/// take the error, and turn the unaccountable process back into no process; see
/// `run::run_spawn_watched`'s `announce_started` and `root::launch_inner`'s `started`.
pub fn record(project: &ProjectDir, kind: RecordKind) {
    let path = project.journal();
    let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    match entry(&mut open, &path) {
        Ok(j) => j.record(kind),
        Err(e) => eprintln!("marion: cannot open the journal {}: {e}", path.display()),
    }
}

/// **§6.1 step 7's confirmation for a managed node, and the `Running` beside it** — one rule for
/// a headless root (`root::confirm_root_started`) and a child (`run::run_spawn_watched`'s
/// `announce_started`), in the shape `native_launch::NativeNodeJournal::spawned` already writes
/// for a native root through the supervisor's own handle.
///
/// **`Running` from the instant the process is held.** Before this record a managed node replayed
/// as `Spawning` for its whole life — `run.rs` and `root.rs` write nothing between `Spawned` and
/// `Exited` — and `marion tree` said `spawning` about a node whose process had been running for
/// minutes. `Spawning` means what `SpawnIntent` alone means: *no process exists*. At this instant
/// one does, marion holds its `Child`, its reader is already draining it (`root.rs`: *"the master,
/// then the recorder, then the child"*; `run.rs`'s driver reads from the first byte), and its pid
/// is §6.7's kill target. The only honest reading of that node is `Running`, and the harness's
/// own turn boundaries — `Blocked`, `Idle` — overwrite it as they are observed. The exit still
/// lands on it: `fold_exited` overrides any live state.
///
/// Two records, two failure policies, stated once. `Spawned` goes through the fallible [`append`]
/// because its loss is not survivable (see [`record`]); the `Running` after it goes through
/// [`record`], because losing it leaves a node marion can still name — stale, not invisible — and
/// killing a live process over a full disk buys nothing against that.
pub fn confirm_spawned(
    project: &ProjectDir,
    spawned: Spawned,
) -> Result<JournalRecord, JournalError> {
    let agent_id = spawned.agent_id.clone();
    let confirmed = append(project, RecordKind::Spawned(spawned))?;
    record(project, running(&agent_id));
    Ok(confirmed)
}

/// The `StateChanged(Running)` a node's confirmation is followed by — see [`confirm_spawned`].
/// One constructor rather than three spellings, so the managed and native lanes cannot drift into
/// different records for the same fact.
pub fn running(agent_id: &AgentId) -> RecordKind {
    RecordKind::StateChanged(StateChanged {
        agent_id: agent_id.clone(),
        state: NodeState::Running,
    })
}

/// **Every permission marion refused on one node, journaled.** Written for a root
/// (`root::launch_and_journal`) and for a child (`run::run_spawn`) by the *same* function, for the
/// reason [`record`] gives about its own failure policy: a record whose shape or destination
/// differed between the two would make the journal's meaning depend on which node it is about.
///
/// **The journal is the only destination, including for a child that has a contract.** §9 puts the
/// denial here rather than in a contract *because a root has none* — but the contract is not the
/// second home the absence of that reason would suggest. §6.7 makes `TaskContract` the audit record
/// of one task's *result*, and §4.3's discipline throughout is one fact, one home: the journal
/// records *that* a contract exists and how it ended, **never its contents**, precisely so nothing
/// is asserted twice by two writers. A denial copied into both would be exactly that second source
/// of truth, and it would be the weaker copy — `marion_core::registry::replay` already folds these
/// records into `ReplayedNode.denied_permissions` keyed on `agent_id`, which reads a child's
/// denials with no change at all, while a contract field would be readable only by whoever already
/// had the contract in hand.
pub fn record_permission_denials(
    project: &ProjectDir,
    agent_id: &AgentId,
    tools: &[String],
    reason: &str,
) {
    for tool in tools {
        record(
            project,
            RecordKind::PermissionDenied(PermissionDenied {
                agent_id: agent_id.clone(),
                tool: tool.clone(),
                reason: reason.to_string(),
            }),
        );
    }
}

/// This process's writer identity: pid plus a UUIDv7, so two runs of one pid (recycled after a
/// reboot, or in a container) never collide.
pub fn writer_id() -> WriterId {
    let unique = match crate::clock::entropy() {
        Ok(e) => marion_core::ids::uuid_v7(crate::clock::unix_millis(), e),
        // Entropy is unavailable only in a state marion cannot run in anyway; the pid alone still
        // separates this writer from a concurrent bridge, which is what the ordinal needs.
        Err(_) => format!("no-entropy-{}", crate::clock::unix_millis()),
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
    use marion_testsupport::{pinned_version, scratch};

    fn intent(id: &str, parent: Option<&str>) -> RecordKind {
        RecordKind::SpawnIntent(SpawnIntent {
            agent_id: AgentId(id.into()),
            parent_id: parent.map(|p| AgentId(p.into())),
            agent_type: "claude".into(),
            harness: Harness::ClaudeCode,
            depth: u32::from(parent.is_some()),
            task_id: parent.map(|_| TaskId("t-1".into())),
            timeout_secs: None,
        })
    }

    #[test]
    fn a_written_journal_replays_to_the_tree_that_was_written() {
        let dir = scratch("journal-round-trip");
        let path = dir.join("journal.jsonl");
        {
            let mut j = Journal::open_path(&path, WriterId("w".into())).unwrap();
            j.append(intent("root", None)).unwrap();
            j.append(RecordKind::Spawned(Spawned {
                agent_id: AgentId("root".into()),
                // From the one table rather than a fourth copy of the literal: a bare "2.1.220"
                // here reads as a claim about the pinned CLI, and a pin that lives in two places
                // is the shape commit 6803b5b removed from the auth wire spelling.
                harness_version: pinned_version("claude").into(),
                model: None,
                pid: Some(1),
                start_id: None,
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
        let dir = scratch("journal-append");
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
        let dir = scratch("journal-barrier");
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
        let dir = scratch("journal-timer");
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
        let dir = scratch("journal-missing");
        let r = read_path(&dir.join("nope.jsonl")).unwrap();
        assert_eq!(r.records, 0);
        assert!(r.nodes().is_empty());
    }

    /// **The pool is what makes the ordinal an ordinal**, and it is reachable only through
    /// [`append_at`] and [`record`] — which is the whole reason [`append_at`] exists rather than
    /// each caller opening its own handle.
    ///
    /// Two independent calls, no shared `Journal` value between them: one writer, `seq` continuing.
    /// Contrast [`a_second_open_appends_rather_than_truncating`] directly above, which opens twice
    /// **on purpose** to prove the file is not truncated — and produces, correctly, two writers each
    /// starting at 0. That is the shape this test forbids for one process's own records.
    #[test]
    fn two_appends_through_the_pool_are_one_writer_continuing_its_sequence() {
        let dir = scratch("journal-pool");
        let path = dir.join("journal.jsonl");
        let first = append_at(&path, intent("root", None)).unwrap();
        let second = append_at(&path, intent("child", Some("root"))).unwrap();
        assert_eq!(
            first.writer, second.writer,
            "one process, one identity — `this_writer` is a `OnceLock` and the handle is pooled"
        );
        assert_eq!((first.seq, second.seq), (0, 1), "the ordinal continues");
        assert!(
            first.mono_ns <= second.mono_ns,
            "and `mono_ns` shares one origin rather than restarting: {} then {}",
            first.mono_ns,
            second.mono_ns
        );
        let r = read_path(&path).unwrap();
        assert_eq!(r.records, 2);
        assert!(
            r.gaps.is_empty(),
            "gapless by construction, which is what makes a gap mean loss"
        );
    }

    /// **A managed node is `Running` from the instant its process is held, and its `Spawned`
    /// carries the version the launch probed** — the two facts `marion tree` showed as `spawning`
    /// and `unknown` for a headless root's whole life, in the shape `native_launch.rs` already
    /// writes for a native root: `Spawned`, then `StateChanged(Running)` beside it.
    ///
    /// Mutations: drop the `Running` from `confirm_spawned` and the summary reads `Spawning` until
    /// the exit; write the version as `"unknown"` and the summary carries no version.
    #[test]
    fn a_confirmed_spawn_replays_running_with_its_probed_version() {
        let dir = scratch("journal-confirm-spawned");
        let project = ProjectDir::new(&dir.join("state"), &dir.join("repo"));
        std::fs::create_dir_all(project.journal().parent().unwrap()).unwrap();
        let agent_id = AgentId("root".into());
        record(&project, intent("root", None));
        confirm_spawned(
            &project,
            Spawned {
                agent_id: agent_id.clone(),
                harness_version: "9.9.9-probed".into(),
                model: None,
                pid: Some(std::process::id() as i32),
                start_id: None,
            },
        )
        .expect("the confirmation lands");

        let replayed = crate::registry::Registry::boot_path(&project.journal()).unwrap();
        let node = replayed.tree().get(&agent_id).expect("the node replays");
        let summary = crate::handler::summarize(node, false).expect("a managed root projects");
        assert_eq!(
            summary.state,
            NodeState::Running,
            "a managed node whose process marion holds is running, not still spawning"
        );
        assert_eq!(
            summary.harness_version.as_deref(),
            Some("9.9.9-probed"),
            "the summary carries the version the launch probed, not a placeholder"
        );

        record(
            &project,
            RecordKind::Exited(Exited {
                agent_id: agent_id.clone(),
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "clean exit".into(),
                },
            }),
        );
        let replayed = crate::registry::Registry::boot_path(&project.journal()).unwrap();
        let node = replayed.tree().get(&agent_id).expect("the node replays");
        assert!(
            node.state.is_exited(),
            "the exit still lands on the running node: {:?}",
            node.state
        );
    }

    #[test]
    fn writer_ids_are_distinct_within_a_process() {
        // Two bridges under one supervisor would otherwise share an identity and read as one
        // writer emitting a gap-ridden sequence.
        assert_ne!(writer_id(), writer_id());
    }
}
