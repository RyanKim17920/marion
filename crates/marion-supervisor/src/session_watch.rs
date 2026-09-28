//! **The one producer of [`RecordKind::SessionObserved`].**
//!
//! A resume hands the harness back the id the harness itself handed out — Claude Code's
//! `system`/`init` `session_id`, codex's `thread.started` `thread_id` — and that id arrives in the
//! node's stream some time after `Spawned` is on disk. So it is its own record, written by whoever
//! owns the node's stream at the instant the frame carrying it goes by, and written **once**: a
//! second sighting is the same session, and a stream that named two would be a harness marion has
//! not measured.
//!
//! Where the id sits is the harness's business — a row's
//! [`marion_harness::grammar::StreamGrammar::session`], or ACP's `session/new` answer — and reading
//! it is the adapter's ([`marion_harness::HarnessAdapter::session_id`]); this module owns only the
//! *when* (first sighting) and the *where to* (the project's journal). No harness is named here.
//!
//! The record goes through [`crate::journal::record`]'s never-fail-the-run policy: a node whose
//! session could not be journaled is a node that cannot be resumed later, and that is a loss to
//! report — not a reason to kill a run that is otherwise fine.

use std::cell::{Cell, RefCell};

use marion_core::contract::{AgentId, Workspace};
use marion_core::harness::Harness;
use marion_core::journal::{RecordKind, SessionObserved};
use marion_core::paths::ProjectDir;
use marion_harness::HarnessAdapter;
use marion_harness::adapter::adapter_for;
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
    /// **Where this node runs**, recorded beside the session for the same reason the shape is: a
    /// resume reconstructs the launch from written values, and a harness resumes a session only
    /// from the cwd it was created in. `None` where the launch did not say — a root's cwd is the
    /// working tree of the project this supervisor is keyed on and is derivable from it, so only
    /// the child path fills this in.
    workspace: Option<Workspace>,
    /// The adapter that reads a session id off a frame, or `None` for a harness with none — nothing
    /// is observed, honestly, and the node replays with `harness_session: None`.
    reader: Option<Box<dyn HarnessAdapter + Send + Sync>>,
    /// The session journaled for this node, once a frame has named one.
    session: RefCell<Option<String>>,
    /// The node's profiles and which attempt is running: the profile's name rides the session
    /// record, and each frame's usage-window reading is kept for `marion profile list`. `None`
    /// on every launch that runs on no profile.
    profiles: Option<&'a crate::profiles::Launch>,
    attempt: Cell<usize>,
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
            workspace: None,
            reader: adapter_for(harness).ok(),
            session: RefCell::new(None),
            profiles: None,
            attempt: Cell::new(0),
        }
    }

    /// **Name the profiles this node runs on.** The profile of the running attempt is written onto
    /// the session record, so a resume continues on the account that owns the conversation.
    pub(crate) fn with_profiles(mut self, profiles: &'a crate::profiles::Launch) -> Self {
        self.profiles = (!profiles.chain.is_empty()).then_some(profiles);
        self
    }

    /// A fresh process of the same node — a profile failover's relaunch — is about to run on
    /// attempt `at`: its session is a new one, so the next sighting is journaled again.
    pub(crate) fn restart(&self, at: usize) {
        self.attempt.set(at);
        *self.session.borrow_mut() = None;
    }

    /// **Name the tree this node runs in.** A separate step rather than a fifth constructor
    /// argument because only one of the two launch paths has an answer: `run_spawn` selects a
    /// workspace and passes it here, while a root's cwd is the project's own working tree and
    /// recording it would be a second copy of a value §2 already derives. A watch nobody calls this
    /// on records `None`, which is what "the journal does not say" is spelled as.
    pub(crate) fn in_workspace(mut self, workspace: Option<Workspace>) -> Self {
        self.workspace = workspace;
        self
    }

    /// One stdout line as it landed. A line that is not JSON is not a frame and carries nothing.
    pub(crate) fn observe_line(&self, line: &str) {
        if self.session.borrow().is_some() && self.profiles.is_none() {
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
        if let Some(profiles) = self.profiles {
            profiles.observe(self.attempt.get(), frame);
        }
        if self.session.borrow().is_some() {
            return;
        }
        let Some(reader) = &self.reader else { return };
        if let Some(id) = reader.session_id(frame) {
            *self.session.borrow_mut() = Some(id.clone());
            crate::journal::record(
                self.project,
                RecordKind::SessionObserved(SessionObserved {
                    agent_id: self.agent_id.clone(),
                    harness: self.harness,
                    session_id: id,
                    pane: self.pane,
                    workspace: self.workspace.clone(),
                    profile: self
                        .profiles
                        .and_then(|p| p.profile(self.attempt.get()))
                        .map(|p| p.name.clone()),
                }),
            );
        }
    }

    /// The session journaled for this node, if a frame has named one — the handle a continuation
    /// relaunches the node under.
    pub(crate) fn session(&self) -> Option<String> {
        self.session.borrow().clone()
    }

    /// Whether a session has been journaled for this node.
    #[cfg(test)]
    pub(crate) fn seen(&self) -> bool {
        self.session.borrow().is_some()
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
        // codex over app-server (S36): the answer to `thread/start` names the thread.
        let watch = SessionWatch::new(&project, &id, Harness::Codex, false);
        watch.observe_line("Reading additional input from stdin...");
        watch.observe_line(r#"{"id":0,"result":{"userAgent":"codex"}}"#);
        watch.observe_line(r#"{"method":"turn/started","params":{"turn":{"id":"u-1"}}}"#);
        assert!(!watch.seen());
        assert_eq!(watch.session(), None);
        assert_eq!(records(&project), 0, "nothing named a session yet");
        watch.observe_line(r#"{"id":1,"result":{"thread":{"id":"t-first"}}}"#);
        assert!(watch.seen());
        watch.observe_line(r#"{"id":1,"result":{"thread":{"id":"t-second"}}}"#);
        watch.observe_event(StreamEvent::Frame(
            &serde_json::json!({"id":1,"result":{"thread":{"id":"t-third"}}}),
        ));
        assert_eq!(records(&project), 1, "one record, on first sighting");
        assert_eq!(sessions(&project), vec!["t-first".to_string()]);
        assert_eq!(
            watch.session().as_deref(),
            Some("t-first"),
            "the session a continuation resumes is the one journaled"
        );
    }

    /// A duplex frame is observed through the same rule as a line, and an ACP node's session is the
    /// one its `session/new` answered with — not the id a later `session/prompt` answer echoes.
    #[test]
    fn a_duplex_frame_is_observed_and_an_acp_session_is_its_session_new_answer() {
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
        watch.observe_line(r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1}}"#);
        watch.observe_line(r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#);
        assert!(!watch.seen(), "nothing named a session yet");
        watch.observe_line(r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"ses_1"}}"#);
        watch.observe_line(r#"{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn"}}"#);
        assert_eq!(sessions(&project), vec!["ses_1".to_string()]);
        assert_eq!(records(&project), 1);
    }

    /// **The workspace the launch chose reaches the record**, so a resume reads where the node ran
    /// instead of deriving it. A child's is the only one that could not be derived at all — it is a
    /// linked worktree marion made under `.marion/worktrees/…` — and it is the one carried here.
    #[test]
    fn the_launch_workspace_is_carried_onto_the_session_record() {
        let (_dir, project) = scratch_project("workspace");
        let id = AgentId("n-4".into());
        let ws = marion_core::contract::Workspace::Worktree {
            path: std::path::PathBuf::from("/p/.marion/worktrees/n-4"),
            branch: "marion/t-4".into(),
        };
        let watch =
            SessionWatch::new(&project, &id, Harness::Codex, false).in_workspace(Some(ws.clone()));
        watch.observe_line(r#"{"id":1,"result":{"thread":{"id":"t-1"}}}"#);
        let bytes = std::fs::read(project.journal()).unwrap_or_default();
        let node = replay(&bytes).get(&id).cloned().expect("the node replays");
        assert_eq!(node.harness_session.as_deref(), Some("t-1"));
        assert_eq!(
            node.launch_workspace,
            Some(ws),
            "the record names the tree the session was created in"
        );

        // A watch nobody told where it was running says `None` rather than naming a directory.
        let (_dir, project) = scratch_project("workspace-absent");
        let watch = SessionWatch::new(&project, &id, Harness::Codex, false);
        watch.observe_line(r#"{"id":1,"result":{"thread":{"id":"t-2"}}}"#);
        let bytes = std::fs::read(project.journal()).unwrap_or_default();
        assert_eq!(replay(&bytes).get(&id).unwrap().launch_workspace, None);
    }
}
