//! **What each node has spent so far, kept current by reading only what its stream appended.**
//!
//! A node's tokens are a fold over the usage units its `events.jsonl` carries, under its row's
//! usage rule. Folding the whole file per question would cost a watcher that asks once a second a
//! read of every byte the node ever said, every second; this keeps, per node, the byte it has read
//! to and the units read so far, and each question reads only the bytes appended since.
//!
//! The file is the source, not a producer's memory: a root started by a `marion run` in another
//! process records to the same file, and a restarted supervisor rebuilds a tally by reading it once.
//! No harness is named here — the rule comes from the node's adapter
//! ([`marion_harness::adapter::HarnessAdapter::usage_rule`]).

use marion_core::contract::{AgentId, TokenUsage};
use marion_core::event::{EventLog, Payload};
use marion_harness::grammar::{UsageRule, fold_usage, turns, usage_units};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;

/// One node's reading so far.
#[derive(Debug, Clone, Default)]
struct Tally {
    /// The byte of `events.jsonl` read to: always the end of a whole line.
    offset: u64,
    units: Vec<TokenUsage>,
    /// The run total those units fold to under the node's rule, kept so a snapshot reads it
    /// without the rule.
    total: Option<u64>,
}

/// What a node's stream says it has spent: the run's usage under its rule's fold, and each turn's
/// total where its units are turns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spent {
    pub usage: Option<TokenUsage>,
    pub turns: Vec<u64>,
}

/// Every node's tally, keyed by agent id.
#[derive(Debug, Default)]
pub struct Tallies {
    nodes: Mutex<HashMap<AgentId, Tally>>,
    /// Ended nodes read since they ended: their total cannot move again. A leaf lock, taken alone.
    settled: Mutex<std::collections::HashSet<AgentId>>,
}

impl Tallies {
    /// Read what `events` appended since the last reading of `id` and answer the node's spend.
    ///
    /// The file is read outside the map's lock, so one slow read holds up no other node; a
    /// concurrent reading of the same node that finished first wins, and this one's is dropped
    /// rather than counting the same bytes twice. A file shorter than the byte read to was
    /// replaced, and is read again from its start.
    pub fn read(&self, id: &AgentId, events: &Path, rule: &UsageRule) -> Spent {
        let before = self.lock().get(id).cloned().unwrap_or_default();
        let mut after = before.clone();
        advance(&mut after, events, rule);
        after.total = fold_usage(rule, &after.units).map(|u| u.total());
        let mut map = self.lock();
        let current = map.entry(id.clone()).or_default();
        if current.offset == before.offset && current.units.len() == before.units.len() {
            *current = after;
        }
        Spent {
            usage: fold_usage(rule, &current.units),
            turns: turns(rule, &current.units),
        }
    }

    /// The run total the last reading of `id` found, without reading anything: what a tree
    /// snapshot shows on a row.
    pub fn total(&self, id: &AgentId) -> Option<u64> {
        self.lock().get(id).and_then(|t| t.total)
    }

    /// Whether `id` ended and has been read since: nothing it could append would move its total.
    pub fn settled(&self, id: &AgentId) -> bool {
        self.settled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(id)
    }

    /// Record that `id` has ended and been read since.
    pub fn settle(&self, id: &AgentId) {
        self.settled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, Tally>> {
        self.nodes.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Fold the whole lines `events` holds past `t.offset` into `t`. A missing or unreadable file is
/// nothing new: a view never fails the answer it rides on.
fn advance(t: &mut Tally, events: &Path, rule: &UsageRule) {
    let Ok(mut file) = std::fs::File::open(events) else {
        return;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len < t.offset {
        *t = Tally::default();
    }
    if len == t.offset {
        return;
    }
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(t.offset)).is_err() || file.read_to_end(&mut bytes).is_err() {
        return;
    }
    let mut read = Vec::new();
    let consumed = EventLog::default().extend(&bytes, &mut read);
    let frames: Vec<serde_json::Value> = read
        .into_iter()
        .filter_map(|e| match e.payload {
            Payload::Vendor { json, .. } => Some(json),
            _ => None,
        })
        .collect();
    t.units.extend(usage_units(rule, &frames));
    t.offset += consumed as u64;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventSink, EventWriter};
    use marion_core::harness::Harness;
    use marion_harness::adapter::adapter_for;

    /// **A tally grows with its file and never counts a byte twice.** Fed codex's recorded stream
    /// in two halves, the running total after the second read equals a whole-file fold, and a
    /// read with nothing appended changes nothing.
    #[test]
    fn a_tally_reads_only_what_was_appended_and_matches_a_whole_file_fold() {
        let dir = marion_testsupport::scratch("usage-tally");
        let path = dir.join("events.jsonl");
        let id = AgentId("n-tally".into());
        let sink = EventSink::new(
            EventWriter::open_path(&path, &id).unwrap(),
            Harness::Codex,
            "unused".into(),
        );
        let adapter = adapter_for(Harness::Codex).unwrap();
        let rule = adapter.usage_rule().expect("codex states its usage");
        let fixture = include_str!("../../../tests/fixtures/s6/exec-mcp-report.stream.jsonl");
        let lines: Vec<&str> = fixture.lines().collect();
        let (first, second) = lines.split_at(lines.len() / 2);
        let tallies = Tallies::default();
        for l in first {
            sink.record_line(l);
        }
        let early = tallies.read(&id, &path, rule);
        for l in second {
            sink.record_line(l);
        }
        let late = tallies.read(&id, &path, rule);
        let again = tallies.read(&id, &path, rule);
        let whole = adapter.usage(&crate::activity::all_frames(&path));
        assert!(whole.is_some(), "the fixture carries usage");
        assert_eq!(late.usage, whole, "incremental equals whole-file");
        assert_eq!(again, late, "nothing appended, nothing changes");
        assert_eq!(
            tallies.total(&id),
            whole.map(|u| u.total()),
            "the cached total needs no read"
        );
        assert!(
            early
                .usage
                .is_none_or(|u| u.total() <= late.usage.unwrap().total())
        );
    }
}
