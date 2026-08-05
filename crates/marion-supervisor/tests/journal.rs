//! The journal's crash and concurrency properties (design §4.3, §7.4, §9's M2 criterion).
//!
//! These are the tests that would notice if the journal stopped being a journal: a torn tail read
//! as corruption, a concurrent writer's record read as a fragment of someone else's, or a replay
//! that quietly reconstructs a different tree than the one that was written.

use std::path::Path;

use marion_core::contract::{AgentId, ExitStatus, ProcessExit, TaskId};
use marion_core::harness::Harness;
use marion_core::journal::{
    Exited, JournalRecord, RecordKind, SpawnIntent, Spawned, StateChanged, WriterId, encode,
};
use marion_core::node::NodeState;
use marion_core::registry::{Truncation, replay};
use marion_supervisor::journal::{Journal, read_path};
use marion_testsupport::scratch;

/// A description carrying multi-byte UTF-8, so the truncation sweep below necessarily cuts one.
const MULTIBYTE: &str = "killed on marion’s bound — 时限到了 ✂";

fn a_tree() -> Vec<RecordKind> {
    let mut kinds = vec![
        RecordKind::SpawnIntent(SpawnIntent {
            agent_id: AgentId("root".into()),
            parent_id: None,
            agent_type: "claude".into(),
            harness: Harness::ClaudeCode,
            depth: 0,
            task_id: None,
        }),
        RecordKind::Spawned(Spawned {
            agent_id: AgentId("root".into()),
            harness_version: "2.1.220".into(),
            model: None,
            pid: Some(101),
        }),
    ];
    for i in 0..3 {
        let child = AgentId(format!("child-{i}"));
        kinds.push(RecordKind::SpawnIntent(SpawnIntent {
            agent_id: child.clone(),
            parent_id: Some(AgentId("root".into())),
            agent_type: "codex-impl".into(),
            harness: Harness::Codex,
            depth: 1,
            task_id: Some(TaskId(format!("t-{i}"))),
        }));
        kinds.push(RecordKind::Spawned(Spawned {
            agent_id: child.clone(),
            harness_version: "0.9.0".into(),
            model: None,
            pid: Some(200 + i),
        }));
        kinds.push(RecordKind::StateChanged(StateChanged {
            agent_id: child.clone(),
            state: NodeState::Running,
        }));
        kinds.push(RecordKind::Exited(Exited {
            agent_id: child.clone(),
            status: ExitStatus::TimedOut,
            exit: ProcessExit {
                code: None,
                signal: Some(9),
                description: MULTIBYTE.into(),
            },
        }));
        kinds.push(RecordKind::ContractPersisted(
            marion_core::journal::ContractPersisted {
                agent_id: child,
                task_id: TaskId(format!("t-{i}")),
                requester: AgentId("root".into()),
                status: Some(ExitStatus::TimedOut),
            },
        ));
    }
    kinds
}

fn write_tree(path: &Path) -> Vec<usize> {
    let mut j = Journal::open_path(path, WriterId("w-torn".into())).unwrap();
    let mut boundaries = vec![0usize];
    for kind in a_tree() {
        let record = j.append(kind).unwrap();
        let len = encode(&record).unwrap().len();
        boundaries.push(boundaries.last().unwrap() + len);
    }
    boundaries
}

/// **Torn tail.** A journal cut at *every* byte offset — mid-record and mid-multi-byte-character
/// included — must replay to the longest intact prefix, never error, never panic.
///
/// §7.4: *"A truncated final line is discarded on replay."* This is that sentence as a sweep.
#[test]
fn a_journal_truncated_at_any_offset_replays_to_its_longest_intact_prefix() {
    let dir = scratch("journal-it-torn");
    let path = dir.join("journal.jsonl");
    let boundaries = write_tree(&path);
    let whole = std::fs::read(&path).unwrap();
    assert_eq!(*boundaries.last().unwrap(), whole.len());

    // The sweep really does cut multi-byte characters: assert it rather than hope.
    let text = std::str::from_utf8(&whole).unwrap();
    let mid_char_offsets: Vec<usize> = (0..whole.len())
        .filter(|k| !text.is_char_boundary(*k))
        .collect();
    assert!(
        mid_char_offsets.len() >= 6,
        "the fixture must contain multi-byte characters for this sweep to mean anything; \
         found {} mid-character offsets",
        mid_char_offsets.len()
    );

    for cut in 0..=whole.len() {
        let prefix = &whole[..cut];
        let got = replay(prefix);

        // The longest intact prefix: every record whose terminating newline is within the cut.
        let intact = boundaries.iter().filter(|b| **b <= cut).count() - 1;
        assert_eq!(
            got.records, intact,
            "cut at {cut}: expected {intact} intact records"
        );
        assert!(
            got.gaps.is_empty(),
            "cut at {cut}: a truncation is not an ordinal gap"
        );

        // …and the tree it yields is exactly the tree the intact prefix records — not merely the
        // right *count*. This is §9's structural criterion applied to a crash-truncated file.
        let expected = replay(&whole[..boundaries[intact]]);
        assert_eq!(
            got.nodes(),
            expected.nodes(),
            "cut at {cut}: a torn tail changed the tree the intact prefix records"
        );

        match (cut == boundaries[intact], &got.truncation) {
            (true, t) => assert!(
                t.is_none(),
                "cut at {cut} is a record boundary, so nothing is torn: {t:?}"
            ),
            (false, Some(Truncation::UnterminatedTail { byte_offset, bytes })) => {
                assert_eq!(*byte_offset, boundaries[intact]);
                assert_eq!(*bytes, cut - boundaries[intact]);
            }
            (false, other) => panic!("cut at {cut}: expected an unterminated tail, got {other:?}"),
        }
    }
}

/// A journal whose tail is not merely short but **garbage** — the shape a torn write from another
/// writer would leave if the fragment were ever glued to a following record.
#[test]
fn a_complete_but_unparsable_line_stops_replay_rather_than_being_skipped() {
    let dir = scratch("journal-it-garbage");
    let path = dir.join("journal.jsonl");
    write_tree(&path);
    let mut bytes = std::fs::read(&path).unwrap();
    let good = replay(&bytes).records;
    bytes.extend_from_slice(b"{\"writer\":\"w\",\"seq\":\r\xff\xfe not json\n");
    bytes.extend_from_slice(
        &encode(&JournalRecord {
            writer: WriterId("w".into()),
            seq: 99,
            ts: marion_core::encoding::SystemTime::from_unix_millis(0),
            mono_ns: 0,
            provenance: marion_core::ir::Provenance::marion(),
            src_seq: None,
            kind: RecordKind::ReapConfirmed(marion_core::journal::ReapConfirmed {
                agent_id: AgentId("root".into()),
            }),
        })
        .unwrap(),
    );
    let r = replay(&bytes);
    assert_eq!(
        r.records, good,
        "replay stops at the first line it cannot read"
    );
    assert!(
        matches!(r.truncation, Some(Truncation::Unparsable { .. })),
        "{:?}",
        r.truncation
    );
}

/// Every byte offset of a **file that is not valid UTF-8 at all** — replay takes bytes, so this
/// must be as boring as the sweep above.
#[test]
fn invalid_utf8_anywhere_is_never_a_panic() {
    let dir = scratch("journal-it-utf8");
    let path = dir.join("journal.jsonl");
    write_tree(&path);
    let mut bytes = std::fs::read(&path).unwrap();
    for (i, b) in bytes.iter_mut().enumerate() {
        if i % 37 == 0 {
            *b = 0xff;
        }
    }
    let r = replay(&bytes);
    assert!(r.records <= 20);
    for cut in 0..bytes.len() {
        let _ = replay(&bytes[..cut]);
    }
}

// ---------------------------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------------------------

const PER_WRITER: u64 = 200;

fn append_many(path: &Path, writer: &str) {
    let mut j = Journal::open_path(path, WriterId(writer.into())).unwrap();
    for i in 0..PER_WRITER {
        j.append(RecordKind::StateChanged(StateChanged {
            agent_id: AgentId(format!("{writer}-{i}")),
            state: NodeState::Running,
        }))
        .unwrap();
        // A barrier every tenth record, so the fsync path is exercised concurrently too.
        if i % 10 == 0 {
            j.append(RecordKind::ReapConfirmed(
                marion_core::journal::ReapConfirmed {
                    agent_id: AgentId(format!("{writer}-{i}")),
                },
            ))
            .unwrap();
        }
    }
}

fn assert_all_present(path: &Path, writers: &[&str]) {
    let r = read_path(path).unwrap();
    let per_writer = PER_WRITER as usize + PER_WRITER.div_ceil(10) as usize;
    assert_eq!(
        r.records,
        per_writer * writers.len(),
        "every record from every writer must survive the interleaving"
    );
    assert_eq!(
        r.truncation, None,
        "a concurrent append must never leave a torn line"
    );
    assert!(
        r.gaps.is_empty(),
        "each writer's ordinals are gapless; interleaving is not loss: {:?}",
        r.gaps
    );
    for w in writers {
        for i in 0..PER_WRITER {
            assert!(
                r.get(&AgentId(format!("{w}-{i}"))).is_some(),
                "{w}-{i} is missing"
            );
        }
    }
}

/// **Two threads, one file, separate `Journal`s** — the in-process half of the concurrency claim.
#[test]
fn two_concurrent_writers_in_one_process_interleave_at_record_granularity() {
    let dir = scratch("journal-it-threads");
    let path = dir.join("journal.jsonl");
    std::thread::scope(|s| {
        for w in ["thread-a", "thread-b"] {
            let p = path.clone();
            s.spawn(move || append_many(&p, w));
        }
    });
    assert_all_present(&path, &["thread-a", "thread-b"]);
}

/// **Two processes, one file** — the half that actually matters, because `marion run` and
/// `marion-supervisor mcp` are separate processes (§10) and no in-process lock could serialise
/// them. The child is this same test binary, re-invoked on the helper below.
#[test]
fn two_concurrent_writer_processes_interleave_at_record_granularity() {
    let dir = scratch("journal-it-processes");
    let path = dir.join("journal.jsonl");
    let exe = std::env::current_exe().expect("a test binary knows its own path");
    let mut kids = Vec::new();
    for w in ["proc-a", "proc-b"] {
        kids.push(
            std::process::Command::new(&exe)
                .args(["--exact", "journal_writer_child", "--nocapture"])
                .env("MARION_JOURNAL_CHILD", &path)
                .env("MARION_JOURNAL_WRITER", w)
                .spawn()
                .expect("re-invoking the test binary"),
        );
    }
    for mut k in kids {
        let status = k.wait().unwrap();
        assert!(status.success(), "a writer process failed: {status:?}");
    }
    assert_all_present(&path, &["proc-a", "proc-b"]);
}

/// The child half of the test above. Inert unless re-invoked with `MARION_JOURNAL_CHILD` set —
/// a plain `cargo test` run reaches it with no environment and it does nothing.
#[test]
fn journal_writer_child() {
    let (Ok(path), Ok(writer)) = (
        std::env::var("MARION_JOURNAL_CHILD"),
        std::env::var("MARION_JOURNAL_WRITER"),
    ) else {
        return;
    };
    append_many(Path::new(&path), &writer);
}
