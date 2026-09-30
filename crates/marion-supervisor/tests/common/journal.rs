//! **Seeding a journal a real supervisor then boots over, and reading one back.**
//!
//! Every seeded record goes through `marion_core`'s own encoder, so a change to the record format
//! breaks the tests that seed rather than letting them write a journal no supervisor reads. The
//! readers replay through marion's own `read_path`, so a test sees the tree the supervisor sees.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use marion_core::contract::{AgentId, ExitStatus, ProcessExit};
use marion_core::harness::Harness;
use marion_core::journal::{Exited, JournalRecord, RecordKind, SpawnIntent, Spawned, WriterId};
use marion_core::node::NodeState;
use marion_core::paths::ProjectDir;
use marion_core::registry::Replay;
use marion_supervisor::journal::read_path;
use marion_supervisor::socket::project_root;
use serde_json::Value;

use super::client::BOUND;

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
        budget: None,
        review_of: None,
        agent_id: id.clone(),
        parent_id: parent.map(|p| AgentId(p.into())),
        agent_type: "codex-impl".into(),
        harness: Harness::Codex,
        depth: u32::from(parent.is_some()),
        task_id: None,
        timeout_secs: None,
        verification: vec![],
        race: None,
        workflow: None,
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

/// The journal at `path`, replayed into the node tree.
pub fn tree(journal: &Path) -> Replay {
    read_path(journal).expect("the journal reads back")
}

/// `id`'s current state, or `None` before its first record.
pub fn state_of(journal: &Path, id: &AgentId) -> Option<NodeState> {
    tree(journal).get(id).map(|n| n.state)
}

pub fn is_exited(journal: &Path, id: &AgentId) -> bool {
    state_of(journal, id).is_some_and(|s| s.is_exited())
}

/// Poll a journal fact under [`BOUND`]. The fact, never the time, is what is asserted.
pub fn wait_for(journal: &Path, what: &str, mut fact: impl FnMut(&Path) -> bool) {
    let deadline = Instant::now() + BOUND;
    while !fact(journal) {
        assert!(
            Instant::now() < deadline,
            "{what} never became true within {BOUND:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Every journal line as raw JSON, in the file's byte order — the journal's total order.
pub fn journal_lines(journal: &Path) -> Vec<Value> {
    std::fs::read_to_string(journal)
        .expect("the journal reads back")
        .lines()
        .map(|l| serde_json::from_str(l).expect("a journal line is JSON"))
        .collect()
}

/// `repo`'s project directory under `state`, keyed the way marion keys it: on the **canonical**
/// project root (`socket::project_root`), not the path the test happened to spell.
pub fn project(state: &Path, repo: &Path) -> ProjectDir {
    ProjectDir::new(state, &project_root(repo))
}

/// The whole journal's bytes for `repo`, read from a second process; empty before it exists.
pub fn journal_bytes(state: &Path, repo: &Path) -> Vec<u8> {
    std::fs::read(project(state, repo).journal()).unwrap_or_default()
}
