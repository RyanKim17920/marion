//! **The in-process `run_spawn` bed: a canned provider and the `Env` pointed at it.**
//!
//! `acp_child.rs`, `no_git.rs`, `verification.rs` and `worktree_reap.rs` each built the same
//! fixture, and a dozen files spelled the same `Env` literal. Both are here once, so a new `Env`
//! field is one edit and a bed cannot quietly point its bridge at a different binary.

use std::path::{Path, PathBuf};

use marion_core::paths::ProjectDir;
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::run::Env;

/// The `Env` `run_spawn` takes for a project at `project_root` whose state lives under `state`:
/// this crate's own supervisor binary as the bridge, and children on the canned credential
/// against `base_url` (`None` where the test never lets a child dial out).
pub fn canned_env(state: &Path, project_root: &Path, base_url: Option<String>) -> Env {
    Env {
        os_sandbox: true,
        project_dir: ProjectDir::new(state, project_root),
        project_root: project_root.to_path_buf(),
        state: state.to_path_buf(),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url,
        auth: marion_harness::Auth::Canned,
    }
}

/// One project, one canned provider, and the `Env` `run_spawn` takes — held together so a test can
/// spawn into the *same* project more than once.
pub struct CannedFixture {
    /// The project root children are spawned into (`SpawnRequest::repo`) — a git repo, or not.
    pub repo: PathBuf,
    pub state: PathBuf,
    pub env: Env,
    /// Held, not dropped: dropping the server closes the port the child talks to.
    _server: CannedServer,
}

/// A canned provider replaying `script` (its request log beside `root`'s other files) and an `Env`
/// for `repo`, with state under `<root>/state`.
pub fn canned_fixture(root: &Path, repo: PathBuf, script: Script) -> CannedFixture {
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: root.join("provider-requests.jsonl"),
        script,
    })
    .expect("the canned provider binds");
    let env = canned_env(&state, &repo, Some(server.base_url()));
    CannedFixture {
        repo,
        state,
        env,
        _server: server,
    }
}
