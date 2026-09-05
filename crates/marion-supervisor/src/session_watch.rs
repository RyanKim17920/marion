//! **The one producer of [`RecordKind::SessionObserved`].**
//!
//! A resume hands the harness back the id the harness itself handed out — Claude Code's
//! `system`/`init` `session_id`, codex's `thread.started` `thread_id` — and that id arrives in the
//! node's stream some time after `Spawned` is on disk. So it is its own record, written by whoever
//! owns the node's stream at the instant the frame carrying it goes by, and written **once**: a
//! second sighting is the same session, and a stream that named two would be a harness marion has
//! not measured.
//!
//! Where the id sits is the row's business ([`marion_harness::grammar::StreamGrammar::session`]),
//! and reading it is [`marion_harness::grammar::session_id`]'s; this module owns only the *when*
//! (first sighting) and the *where to* (the project's journal). No harness is named here.
//!
//! The record goes through [`crate::journal::record`]'s never-fail-the-run policy: a node whose
//! session could not be journaled is a node that cannot be resumed later, and that is a loss to
//! report — not a reason to kill a run that is otherwise fine.

use std::cell::Cell;

use marion_core::contract::AgentId;
use marion_core::harness::Harness;
use marion_core::journal::{RecordKind, SessionObserved};
use marion_core::paths::ProjectDir;
use marion_harness::adapter::harness_spec;
use marion_harness::grammar::{StreamGrammar, session_id};
use serde_json::Value;

use crate::duplex::StreamEvent;

/// One node's watch for the frame that names its harness session.
pub(crate) struct SessionWatch<'a> {
    project: &'a ProjectDir,
    agent_id: &'a AgentId,
    harness: Harness,
    /// The shape this node was launched in, recorded onto the session so a resume reconstructs it
    /// rather than inferring it. A pane emits no `stream-json`, so today this is only ever seen
    /// beside a `false`; the field is written so a pane node's resume stays a checked refusal.
    pane: bool,
    /// The row's grammar, or `None` for a harness whose stream is read as code (ACP) — nothing is
    /// observed, honestly, and the node replays with `harness_session: None`.
    grammar: Option<&'static StreamGrammar>,
    seen: Cell<bool>,
}

impl<'a> SessionWatch<'a> {
    pub(crate) fn new(
        project: &'a ProjectDir,
        agent_id: &'a AgentId,
        harness: Harness,
        pane: bool,
    ) -> Self {
        Self {
            project,
            agent_id,
            harness,
            pane,
            grammar: harness_spec(harness).stream,
            seen: Cell::new(false),
        }
    }

    /// One stdout line as it landed. A line that is not JSON is not a frame and carries nothing.
    pub(crate) fn observe_line(&self, line: &str) {
        if self.seen.get() {
            return;
        }
        if let Ok(frame) = serde_json::from_str::<Value>(line) {
            self.observe_frame(&frame);
        }
    }

    /// One frame of a duplex stream, as the driver hands it to every sink.
    pub(crate) fn observe_event(&self, ev: StreamEvent<'_>) {
        if let StreamEvent::Frame(frame) = ev {
            self.observe_frame(frame);
        }
    }

    /// One parsed frame. Journals the session the first time a frame names one; every later frame
    /// is ignored without being read.
    pub(crate) fn observe_frame(&self, frame: &Value) {
        if self.seen.get() {
            return;
        }
        let Some(grammar) = self.grammar else { return };
        if let Some(id) = session_id(grammar, frame) {
            self.seen.set(true);
            crate::journal::record(
                self.project,
                RecordKind::SessionObserved(SessionObserved {
                    agent_id: self.agent_id.clone(),
                    harness: self.harness,
                    session_id: id,
                    pane: self.pane,
                }),
            );
        }
    }

    /// Whether a session has been journaled for this node.
    #[cfg(test)]
    pub(crate) fn seen(&self) -> bool {
        self.seen.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::registry::replay;

    fn scratch_project(tag: &str) -> (marion_testsupport::Scratch, ProjectDir) {
        let dir = marion_testsupport::scratch(&format!("session-watch-{tag}"));
        let project = ProjectDir::new(&dir.join("state"), &dir.join("repo"));
        std::fs::create_dir_all(project.path()).unwrap();
        (dir, project)
    }

    fn sessions(project: &ProjectDir) -> Vec<String> {
        let bytes = std::fs::read(project.journal()).unwrap_or_default();
        replay(&bytes)
            .nodes()
            .iter()
            .filter_map(|n| n.harness_session.clone())
            .collect()
    }

    fn records(project: &ProjectDir) -> usize {
        std::fs::read_to_string(project.journal())
            .unwrap_or_default()
            .lines()
            .count()
    }

    /// The first frame that names the session is journaled, once; noise before it and frames after
    /// it — including a second, different id — write nothing.
    #[test]
    fn the_first_sighting_is_journaled_once_and_nothing_else_is() {
        let (_dir, project) = scratch_project("once");
        let id = AgentId("n-1".into());
        let watch = SessionWatch::new(&project, &id, Harness::Codex, false);
        watch.observe_line("Reading additional input from stdin...");
        watch.observe_line(r#"{"type":"turn.started"}"#);
        assert!(!watch.seen());
        assert_eq!(records(&project), 0, "nothing named a session yet");
        watch.observe_line(r#"{"type":"thread.started","thread_id":"t-first"}"#);
        assert!(watch.seen());
        watch.observe_line(r#"{"type":"thread.started","thread_id":"t-second"}"#);
        watch.observe_event(StreamEvent::Frame(
            &serde_json::json!({"type":"thread.started","thread_id":"t-third"}),
        ));
        assert_eq!(records(&project), 1, "one record, on first sighting");
        assert_eq!(sessions(&project), vec!["t-first".to_string()]);
    }

    /// A duplex frame is observed through the same rule as a line, and a harness with no grammar
    /// row observes nothing rather than guessing.
    #[test]
    fn a_duplex_frame_is_observed_and_a_rowless_harness_observes_nothing() {
        let (_dir, project) = scratch_project("duplex");
        let id = AgentId("n-2".into());
        let watch = SessionWatch::new(&project, &id, Harness::ClaudeCode, false);
        let init = serde_json::json!({"type":"system","subtype":"init","session_id":"c-1"});
        watch.observe_event(StreamEvent::Unparsed("warming up"));
        watch.observe_event(StreamEvent::Frame(&init));
        assert_eq!(sessions(&project), vec!["c-1".to_string()]);

        let (_dir, project) = scratch_project("acp");
        let id = AgentId("n-3".into());
        let watch = SessionWatch::new(&project, &id, Harness::Acp, false);
        watch.observe_line(r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"s-1"}}"#);
        assert!(!watch.seen());
        assert_eq!(records(&project), 0);
    }
}
