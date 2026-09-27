//! **Seeding a journal a real supervisor then boots over.**
//!
//! Every record goes through `marion_core`'s own encoder, so a change to the record format breaks
//! the tests that seed rather than letting them write a journal no supervisor reads.

use std::io::Write;
use std::path::Path;

use marion_core::contract::{AgentId, ExitStatus, ProcessExit};
use marion_core::harness::Harness;
use marion_core::journal::{Exited, JournalRecord, RecordKind, SpawnIntent, Spawned, WriterId};

/// Append one record at `seq` to the journal at `path`, creating it and its directory.
pub fn seed(path: &Path, seq: u64, kind: RecordKind) {
    let bytes = marion_core::journal::encode(&JournalRecord {
        writer: WriterId("test".into()),
        seq,
        ts: marion_core::encoding::SystemTime::from_unix_millis(1_000 + seq),
        mono_ns: seq,
        provenance: marion_core::ir::Provenance::marion(),
        src_seq: None,
        kind,
    })
    .expect("a record encodes");
    std::fs::create_dir_all(path.parent().expect("a journal has a directory")).expect("dir");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("journal")
        .write_all(&bytes)
        .expect("append");
}

/// A whole finished life — intent, confirmation, exit — as three records from `*seq` on, which is
/// advanced past them. No pid: the process is long gone, and a supervisor booting over the journal
/// has nothing to look for.
pub fn a_finished_node(
    path: &Path,
    seq: &mut u64,
    agent: &str,
    parent: Option<&str>,
    status: ExitStatus,
) {
    let id = AgentId(agent.into());
    let mut next = |kind| {
        seed(path, *seq, kind);
        *seq += 1;
    };
    next(RecordKind::SpawnIntent(SpawnIntent {
        agent_id: id.clone(),
        parent_id: parent.map(|p| AgentId(p.into())),
        agent_type: "codex-impl".into(),
        harness: Harness::Codex,
        depth: u32::from(parent.is_some()),
        task_id: None,
        timeout_secs: None,
        verification: vec![],
    }));
    next(RecordKind::Spawned(Spawned {
        agent_id: id.clone(),
        harness_version: "0.146.0".into(),
        model: None,
        pid: None,
        start_id: None,
        provider: None,
        route: None,
        credential: None,
    }));
    next(RecordKind::Exited(Exited {
        agent_id: id,
        status,
        exit: ProcessExit {
            code: Some(i32::from(status != ExitStatus::Ok)),
            signal: None,
            description: format!("seeded {status:?}"),
        },
    }));
}

/// Every record in the journal at `path`, decoded with `marion_core`'s own decoder, in order.
/// A missing journal is no records.
pub fn records(path: &Path) -> Vec<RecordKind> {
    std::fs::read(path)
        .unwrap_or_default()
        .split(|b| *b == b'\n')
        .filter_map(marion_core::journal::decode)
        .map(|r| r.kind)
        .collect()
}

/// `(message_id, via)` of every `MessageDelivered` for `agent`.
pub fn delivered_to(path: &Path, agent: &AgentId) -> Vec<(String, String)> {
    records(path)
        .into_iter()
        .filter_map(|k| match k {
            RecordKind::MessageDelivered(d) if &d.agent_id == agent => Some((d.message_id, d.via)),
            _ => None,
        })
        .collect()
}
