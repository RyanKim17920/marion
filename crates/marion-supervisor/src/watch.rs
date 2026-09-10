//! The journal's **first production reader** (§4.2, §10, `MILESTONES.md`'s *"written is not read"*).
//!
//! Every child transition marion makes is already on disk — `SpawnIntent`, `Spawned`,
//! `SpawnAborted`, `Exited`, `ContractPersisted`, `PermissionDenied` — written by the *bridge's*
//! process, which is where a child is actually driven. Nothing at runtime had ever read any of it
//! back. That is why `marion run` could show a root calling `spawn` and then nothing at all for the
//! child's whole life: the events existed, in a file, with no reader.
//!
//! This is that reader, and it is deliberately the smallest one that closes the gap: a byte cursor
//! over the append-only file, [`marion_core::registry::replay`] on the bytes that are new since the
//! last poll, and a memory of what has already been announced.
//!
//! # It is a viewer, and a viewer may never affect the run
//!
//! Every failure mode here degrades to **showing less**, never to failing:
//!
//! - the file does not exist yet, or cannot be opened, or a read fails → nothing this poll, try
//!   again on the next one. A journal that appears late is the normal case, not an error;
//! - the tail is a half-written record → the cursor stops at the last intact newline and re-reads
//!   the torn bytes next poll, which is what `Truncation::UnterminatedTail` is for. The writer is
//!   another process appending, so reading a partial line is expected, not exceptional;
//! - a complete line that is not a record → the watch **stops**, once, saying so. `replay` treats
//!   that as corruption rather than a torn write (an append-only file cannot heal a bad line), and
//!   a viewer that kept narrating past bytes it does not understand would be inventing a tree;
//! - a record kind this module has no rendering for → it contributes to the replayed state and
//!   produces no event, which is the one silence that is correct here: the *node's* start and end
//!   are what this shows, and every kind either moves those or does not.
//!
//! Nothing in this module panics, returns an error, or writes anything anywhere.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use marion_core::contract::{AgentId, ExitStatus, ProcessExit};
use marion_core::harness::Harness;
use marion_core::registry::{Replay, Truncation};

/// One thing that happened to a **child**, worth a line on a watcher's terminal.
///
/// Deliberately not the journal's own record vocabulary: a `SpawnIntent` followed by a `Spawned` is
/// one event to a person ("a child started"), and §6.1's two-step intent/confirmation split is a
/// crash-safety property, not news. Rendering lives at the call site; this is the decision about
/// *what happened*, which is what can be tested without a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildEvent {
    /// A child appeared in the journal. `agent_type` and `harness` are `None` only if the node's
    /// intent record is not in the bytes this watch has read — an honest absence rather than a
    /// guess.
    Started {
        agent_id: AgentId,
        agent_type: Option<String>,
        harness: Option<Harness>,
        depth: Option<u32>,
        pid: Option<i32>,
    },
    /// The intent's other resolution: marion started nothing, or abandoned what it started.
    Aborted {
        agent_id: AgentId,
        agent_type: Option<String>,
        reason: String,
    },
    /// The node's terminal transition, with the status the journal recorded.
    Exited {
        agent_id: AgentId,
        agent_type: Option<String>,
        status: Option<ExitStatus>,
        exit: Option<ProcessExit>,
    },
    /// §9's dead end, happening to a child: marion denied a permission because it has nobody to
    /// ask. Recorded in the journal precisely because a root has no contract to record it in.
    Denied {
        agent_id: AgentId,
        agent_type: Option<String>,
        tool: String,
        reason: String,
    },
    /// The watch gave up, and why. Emitted at most once, and the last thing this watch ever
    /// produces — a viewer that stops must say so, because a view that silently stops updating is
    /// indistinguishable from a run in which nothing further happened.
    Stopped { reason: String },
}

/// What has already been said about one node, so nothing is said twice.
#[derive(Debug, Default)]
struct Announced {
    agent_type: Option<String>,
    started: bool,
    aborted: bool,
    exited: bool,
    /// How many of this node's denials have been reported. The journal's list only grows.
    denials: usize,
}

/// A byte cursor over one project's `journal.jsonl`.
///
/// **Append-only is what makes a cursor sufficient** (§4.2): records are only ever added at the
/// end, so "what is new" is exactly "the bytes past the offset", and nothing already read can
/// change. No seeking, no rewriting, and no lock — the writer's `O_APPEND` writes are atomic under
/// the record cap, so the worst a reader sees is a final line that is not finished yet.
#[derive(Debug)]
pub struct JournalWatch {
    path: PathBuf,
    /// Bytes consumed. Always a line boundary: a torn tail advances nothing.
    offset: u64,
    seen: HashMap<String, Announced>,
    /// The node this watch is **not** about — `marion run`'s own root, whose banner has already
    /// announced it and whose frames the caller is streaming directly. Its children are the point.
    ignore: AgentId,
    stopped: bool,
}

impl JournalWatch {
    /// Watch `path` from **its current end**, ignoring everything about `root`.
    ///
    /// Starting at the end rather than at zero is what keeps one run's view free of every previous
    /// run against the same project: a project's journal is a forest, and the other trees in it are
    /// history, not news. A file that does not exist yet starts at zero, which is the same thing.
    pub fn at_end(path: &Path, root: AgentId) -> Self {
        let offset = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        Self {
            path: path.to_path_buf(),
            offset,
            seen: HashMap::new(),
            ignore: root,
            stopped: false,
        }
    }

    /// Everything that has happened since the last call, in journal order.
    ///
    /// Empty is the overwhelmingly common answer, and it costs one `read` of zero bytes.
    pub fn poll(&mut self) -> Vec<ChildEvent> {
        if self.stopped {
            return Vec::new();
        }
        let Some(fresh) = self.read_new() else {
            // Unreadable, missing, or nothing new. All three are "no news", and none of them is
            // this viewer's business to escalate.
            return Vec::new();
        };
        if fresh.is_empty() {
            return Vec::new();
        }
        // How far the cursor may advance: the intact prefix, and no further. A torn tail is re-read
        // next poll — the writer is still writing it. **`Replay::extend` answers that**, rather
        // than this module matching on `Truncation` itself, because the supervisor's registry
        // tails the same file under the same rule and two copies of it could drift.
        //
        // A fresh `Replay` per poll, deliberately: this is a *view of one run*, and the per-node
        // memory it needs is `Announced`, not the forest. The registry is the reader that keeps one
        // `Replay` across polls, and it is a different object for that reason (see `registry.rs`).
        let mut seen = Replay::default();
        self.offset += seen.extend(&fresh) as u64;

        let mut out = Vec::new();
        for node in seen.nodes() {
            if node.agent_id == self.ignore {
                continue;
            }
            let known = self.seen.entry(node.agent_id.0.clone()).or_default();
            // An intent seen in any chunk is remembered for every later one: a node's identity
            // arrives once, and its exit may be thousands of records later.
            if let Some(t) = node.agent_type() {
                known.agent_type = Some(t.to_string());
            }
            let agent_type = known.agent_type.clone();
            if !known.started {
                known.started = true;
                out.push(ChildEvent::Started {
                    agent_id: node.agent_id.clone(),
                    agent_type: agent_type.clone(),
                    harness: node.harness(),
                    depth: node.depth(),
                    pid: node.pid,
                });
            }
            for denial in node.denied_permissions.iter().skip(known.denials) {
                out.push(ChildEvent::Denied {
                    agent_id: node.agent_id.clone(),
                    agent_type: agent_type.clone(),
                    tool: denial.tool.clone(),
                    reason: denial.reason.clone(),
                });
            }
            known.denials = node.denied_permissions.len();
            if let Some(reason) = &node.spawn_aborted
                && !known.aborted
            {
                known.aborted = true;
                out.push(ChildEvent::Aborted {
                    agent_id: node.agent_id.clone(),
                    agent_type: agent_type.clone(),
                    reason: reason.clone(),
                });
            }
            if node.state.is_exited() && !known.exited {
                known.exited = true;
                out.push(ChildEvent::Exited {
                    agent_id: node.agent_id.clone(),
                    agent_type,
                    status: node.exit_status(),
                    exit: node.exit.clone(),
                });
            }
        }

        if let Some(Truncation::Unparsable { line, .. }) = &seen.truncation {
            // Corruption, not a torn write. Stop, and say so: a view that goes quiet on its own is
            // the failure this whole change exists to remove.
            self.stopped = true;
            out.push(ChildEvent::Stopped {
                reason: format!(
                    "line {} of {} is not a journal record, so marion stopped following it; the \
                     run is unaffected and the journal itself is still the record",
                    line + 1,
                    self.path.display()
                ),
            });
        }
        out
    }

    /// The bytes past the cursor, or `None` if there is nothing to read or nothing readable.
    fn read_new(&self) -> Option<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&self.path).ok()?;
        let len = f.metadata().ok()?.len();
        if len <= self.offset {
            // Equal is the common case. **Shorter** means the file was replaced or rewritten under
            // us, which an append-only journal never does — so this viewer reads nothing rather
            // than guessing at an offset into a file it no longer recognises.
            return None;
        }
        f.seek(SeekFrom::Start(self.offset)).ok()?;
        let mut buf = Vec::with_capacity((len - self.offset) as usize);
        f.take(len - self.offset).read_to_end(&mut buf).ok()?;
        Some(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::journal::{
        Exited, JournalRecord, PermissionDenied, RecordKind, SpawnAborted, SpawnIntent, Spawned,
        WriterId, encode,
    };
    use marion_testsupport::{append, scratch};

    fn root_id() -> AgentId {
        AgentId("00000000-0000-7000-8000-00000000root".into())
    }

    fn child_id(n: u8) -> AgentId {
        AgentId(format!("00000000-0000-7000-8000-00000000000{n}"))
    }

    /// A record, encoded the way the journal encodes one. Built through the real types so a change
    /// to the wire shape breaks this test rather than silently making the watch blind.
    fn line(seq: u64, kind: RecordKind) -> Vec<u8> {
        let record = JournalRecord {
            seq,
            writer: WriterId("test-writer".into()),
            ts: marion_core::encoding::SystemTime::from_unix_millis(1_785_625_628_619),
            mono_ns: seq,
            provenance: marion_core::ir::Provenance::marion(),
            src_seq: None,
            kind,
        };
        let mut bytes = encode(&record).expect("a record encodes");
        bytes.push(b'\n');
        bytes
    }

    fn intent(agent_id: AgentId, parent: Option<AgentId>, agent_type: &str) -> RecordKind {
        RecordKind::SpawnIntent(SpawnIntent {
            agent_id,
            parent_id: parent,
            agent_type: agent_type.into(),
            harness: Harness::Codex,
            depth: 1,
            task_id: None,
        })
    }

    fn exited(agent_id: AgentId, status: ExitStatus) -> RecordKind {
        RecordKind::Exited(Exited {
            agent_id,
            status,
            exit: ProcessExit {
                code: Some(0),
                signal: None,
                description: "child exited with code 0".into(),
            },
        })
    }

    /// The property the whole module exists for: a child's start and end are **read back out of the
    /// journal** as they are written, each exactly once, without the previous poll's events being
    /// repeated.
    #[test]
    fn a_childs_start_and_end_are_reported_once_each_as_they_are_appended() {
        let dir = scratch("watch-basic");
        let path = dir.join("journal.jsonl");
        append(
            &path,
            &line(1, intent(child_id(1), Some(root_id()), "codex")),
        );
        let mut watch = JournalWatch::at_end(&path, root_id());
        // `at_end`: the record written before the watch existed belongs to the past.
        assert_eq!(watch.poll(), Vec::new(), "history is not news");

        append(
            &path,
            &line(2, intent(child_id(2), Some(root_id()), "codex")),
        );
        let events = watch.poll();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(
            &events[0],
            ChildEvent::Started { agent_id, agent_type, harness, .. }
                if *agent_id == child_id(2)
                    && agent_type.as_deref() == Some("codex")
                    && *harness == Some(Harness::Codex)
        ));
        // **The re-render check.** Nothing new was written, so nothing is said.
        assert_eq!(watch.poll(), Vec::new());

        append(
            &path,
            &line(
                3,
                RecordKind::Spawned(Spawned {
                    agent_id: child_id(2),
                    harness_version: "0.146.0".into(),
                    model: None,
                    pid: Some(4242),
                    start_id: None,
                }),
            ),
        );
        assert_eq!(
            watch.poll(),
            Vec::new(),
            "a start already announced is not announced again when it is confirmed"
        );

        append(&path, &line(4, exited(child_id(2), ExitStatus::Ok)));
        let events = watch.poll();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(
            matches!(
                &events[0],
                ChildEvent::Exited { agent_id, agent_type, status, .. }
                    if *agent_id == child_id(2)
                        // Learned from a chunk read long before this one.
                        && agent_type.as_deref() == Some("codex")
                        && *status == Some(ExitStatus::Ok)
            ),
            "{events:?}"
        );
        assert_eq!(watch.poll(), Vec::new(), "and never again");
    }

    /// The root's own records are in the same file — `marion run` writes them — and the banner has
    /// already announced it. The children are the news.
    #[test]
    fn the_watched_runs_own_root_is_not_narrated_back_at_the_person_watching_it() {
        let dir = scratch("watch-root");
        let path = dir.join("journal.jsonl");
        let mut watch = JournalWatch::at_end(&path, root_id());
        append(&path, &line(1, intent(root_id(), None, "claude")));
        append(&path, &line(2, exited(root_id(), ExitStatus::Ok)));
        assert_eq!(watch.poll(), Vec::new());
        // A *different* root — another run against the same project — is not this run's business
        // either, but it is also not in the bytes: `at_end` started past it.
        append(
            &path,
            &line(3, intent(child_id(1), Some(root_id()), "codex")),
        );
        assert_eq!(watch.poll().len(), 1);
    }

    /// **A torn tail is the normal case, not corruption**: the writer is another process appending.
    /// The cursor stops at the last intact newline and the half-written record is read once, when
    /// it is finished — never twice, and never as garbage.
    #[test]
    fn a_record_read_while_it_is_being_written_is_shown_once_when_it_is_complete() {
        let dir = scratch("watch-torn");
        let path = dir.join("journal.jsonl");
        let mut watch = JournalWatch::at_end(&path, root_id());
        let whole = line(1, intent(child_id(1), Some(root_id()), "codex"));
        let (head, tail) = whole.split_at(whole.len() / 2);
        append(&path, head);
        assert_eq!(
            watch.poll(),
            Vec::new(),
            "half a record is not an event, and must not be one"
        );
        append(&path, tail);
        let events = watch.poll();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(&events[0], ChildEvent::Started { .. }));
        assert_eq!(watch.poll(), Vec::new(), "and not a second time");
    }

    /// A complete line that is not a record is corruption — `replay` stops there and so does this.
    /// It says so once and then stays quiet, rather than narrating a tree from bytes it does not
    /// understand or going silently dead.
    #[test]
    fn a_journal_with_a_garbage_line_stops_the_watch_loudly_and_never_panics() {
        let dir = scratch("watch-garbage");
        let path = dir.join("journal.jsonl");
        let mut watch = JournalWatch::at_end(&path, root_id());
        append(
            &path,
            &line(1, intent(child_id(1), Some(root_id()), "codex")),
        );
        append(&path, b"this is not a journal record at all\n");
        append(&path, &line(2, exited(child_id(1), ExitStatus::Ok)));
        let events = watch.poll();
        // The intact prefix is still shown: everything before the bad line is a record.
        assert!(
            matches!(&events[0], ChildEvent::Started { .. }),
            "{events:?}"
        );
        let stopped = events
            .iter()
            .find_map(|e| match e {
                ChildEvent::Stopped { reason } => Some(reason.clone()),
                _ => None,
            })
            .expect("the watch says it stopped");
        assert!(stopped.contains("not a journal record"), "{stopped}");
        assert!(
            stopped.contains("the run is unaffected"),
            "a viewer's failure must not read like the run's: {stopped}"
        );
        // Silent from here, whatever else is written — including the exit it will now never show.
        append(&path, &line(3, exited(child_id(1), ExitStatus::Failed)));
        assert_eq!(watch.poll(), Vec::new());
    }

    /// Every other way a file can refuse to be read, none of which is an error to a viewer.
    #[test]
    fn a_missing_unreadable_or_shrinking_journal_produces_nothing_and_never_fails() {
        let dir = scratch("watch-missing");
        let path = dir.join("nothing-here.jsonl");
        let mut watch = JournalWatch::at_end(&path, root_id());
        assert_eq!(
            watch.poll(),
            Vec::new(),
            "a journal that does not exist yet"
        );
        assert_eq!(watch.poll(), Vec::new(), "and still does not");

        // A directory where a file should be: `open` succeeds on some platforms and the read
        // fails on all of them. Either way, nothing.
        let as_dir = dir.join("journal-is-a-directory");
        std::fs::create_dir_all(&as_dir).unwrap();
        let mut watch = JournalWatch::at_end(&as_dir, root_id());
        assert_eq!(watch.poll(), Vec::new());

        // A file that got *shorter* is not an append-only journal any more. The watch reads
        // nothing rather than guessing at an offset into a file it no longer recognises.
        let path = dir.join("shrinks.jsonl");
        append(
            &path,
            &line(1, intent(child_id(1), Some(root_id()), "codex")),
        );
        append(&path, &line(2, exited(child_id(1), ExitStatus::Ok)));
        let mut watch = JournalWatch::at_end(&path, root_id());
        std::fs::write(&path, b"").unwrap();
        assert_eq!(watch.poll(), Vec::new());
    }

    /// A denial happening to a **child** — §9's dead end, one layer down, which until now was
    /// invisible to a person for the same reason everything else about a child was.
    #[test]
    fn each_denied_permission_is_reported_once_and_an_abort_is_not_an_exit() {
        let dir = scratch("watch-denials");
        let path = dir.join("journal.jsonl");
        let mut watch = JournalWatch::at_end(&path, root_id());
        append(
            &path,
            &line(1, intent(child_id(1), Some(root_id()), "codex")),
        );
        append(
            &path,
            &line(
                2,
                RecordKind::PermissionDenied(PermissionDenied {
                    agent_id: child_id(1),
                    tool: "mcp__marion__spawn".into(),
                    reason: "depth ceiling".into(),
                }),
            ),
        );
        let events = watch.poll();
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(
            &events[1],
            ChildEvent::Denied { tool, .. } if tool == "mcp__marion__spawn"
        ));
        assert_eq!(watch.poll(), Vec::new(), "not repeated");

        append(
            &path,
            &line(
                3,
                RecordKind::SpawnAborted(SpawnAborted {
                    agent_id: child_id(1),
                    reason: "the bridge never became ready".into(),
                }),
            ),
        );
        let events = watch.poll();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(
            matches!(&events[0], ChildEvent::Aborted { reason, .. } if reason.contains("bridge")),
            "an abort is its own event: the child never ran, so it did not exit — {events:?}"
        );
    }
}
