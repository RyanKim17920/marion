//! **What the home screen remembers between runs**: which projects marion has served, and which
//! models each harness was last run on — so Start can offer them instead of asking again.
//!
//! Two small files, both a view and never an authority: a missing, unreadable or malformed file
//! reads as "nothing remembered", and a write that fails is dropped, because forgetting a model
//! name must never stop a run.
//!
//! * `<state>/<project-hash>/project.json` — `{"root": "<canonical root>", "last_used": <unix s>}`,
//!   written when the home screen opens a project. The hash is one-way, so this file is the only
//!   way to list projects by path.
//! * `<state>/recent.json` — `{"models": {"<harness>": ["newest", …]}}`, at most [`MAX_MODELS`]
//!   per harness. **Models only, never prompts**: marion does not persist what an operator typed
//!   (the journal keeps a message's length and digest, not its text), and a history file would be
//!   a second copy of every prompt.
//!
//! Each write goes to a sibling temporary file and is renamed over the old one, so a reader never
//! sees half a file.

use marion_core::paths::ProjectDir;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Models remembered per harness.
pub const MAX_MODELS: usize = 8;

/// One project marion has served.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub root: PathBuf,
    /// Unix seconds.
    pub last_used: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Recent {
    #[serde(default)]
    models: BTreeMap<String, Vec<String>>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

use crate::private_fs::write_atomic;

/// Remember that the project at `root` (canonical) was opened now.
pub fn touch_project(project: &ProjectDir, root: &Path) {
    let p = Project {
        root: root.to_path_buf(),
        last_used: now(),
    };
    if let Ok(bytes) = serde_json::to_vec(&p) {
        let _ = write_atomic(&project.path().join("project.json"), &bytes);
    }
}

/// Every project under `state` that has a `project.json`, most recently used first.
pub fn projects(state: &Path) -> Vec<Project> {
    let Ok(dirs) = std::fs::read_dir(state) else {
        return Vec::new();
    };
    let mut out: Vec<Project> = dirs
        .flatten()
        .filter_map(|d| std::fs::read(d.path().join("project.json")).ok())
        .filter_map(|b| serde_json::from_slice(&b).ok())
        .collect();
    out.sort_by(|a, b| b.last_used.cmp(&a.last_used).then(a.root.cmp(&b.root)));
    out
}

fn recent_path(state: &Path) -> PathBuf {
    state.join("recent.json")
}

fn load(state: &Path) -> Recent {
    std::fs::read(recent_path(state))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Remember that `harness` was just run on `model`: it moves to the front of that harness's list.
pub fn remember_model(state: &Path, harness: &str, model: &str) {
    let model = model.trim();
    if model.is_empty() {
        return;
    }
    let mut r = load(state);
    let list = r.models.entry(harness.to_string()).or_default();
    list.retain(|m| m != model);
    list.insert(0, model.to_string());
    list.truncate(MAX_MODELS);
    if let Ok(bytes) = serde_json::to_vec_pretty(&r) {
        let _ = write_atomic(&recent_path(state), &bytes);
    }
}

/// The models `harness` was run on, most recent first.
pub fn models(state: &Path, harness: &str) -> Vec<String> {
    load(state).models.remove(harness).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_are_most_recent_first_deduplicated_and_bounded() {
        let state = marion_testsupport::scratch("recent-models");
        assert!(
            models(&state, "claude").is_empty(),
            "nothing remembered yet"
        );
        remember_model(&state, "claude", "opus");
        remember_model(&state, "claude", "sonnet");
        remember_model(&state, "claude", "opus");
        remember_model(&state, "codex", "gpt-5.4");
        remember_model(&state, "claude", "  ");
        assert_eq!(models(&state, "claude"), ["opus", "sonnet"]);
        assert_eq!(models(&state, "codex"), ["gpt-5.4"]);
        for i in 0..20 {
            remember_model(&state, "claude", &format!("m{i}"));
        }
        assert_eq!(models(&state, "claude").len(), MAX_MODELS);
        assert_eq!(models(&state, "claude")[0], "m19");
    }

    #[test]
    fn a_malformed_file_is_nothing_remembered_and_is_repaired_by_the_next_write() {
        let state = marion_testsupport::scratch("recent-malformed");
        std::fs::write(recent_path(&state), b"{not json").unwrap();
        assert!(models(&state, "claude").is_empty());
        remember_model(&state, "claude", "opus");
        assert_eq!(models(&state, "claude"), ["opus"]);
    }

    #[test]
    fn projects_are_listed_by_their_recorded_root_newest_first() {
        let state = marion_testsupport::scratch("recent-projects");
        let a = ProjectDir::new(&state, Path::new("/work/a"));
        let b = ProjectDir::new(&state, Path::new("/work/b"));
        touch_project(&a, Path::new("/work/a"));
        write_atomic(
            &b.path().join("project.json"),
            br#"{"root":"/work/b","last_used":1}"#,
        )
        .unwrap();
        // A project directory with no project.json (served before this file existed) is skipped.
        std::fs::create_dir_all(state.join("0123456789ab")).unwrap();
        let got: Vec<PathBuf> = projects(&state).into_iter().map(|p| p.root).collect();
        assert_eq!(got, [PathBuf::from("/work/a"), PathBuf::from("/work/b")]);
    }
}
