//! **Endpoint mode, end to end**: a node whose model names a provider is pointed at that provider
//! with the key the user stored — here, marion's own canned server standing in as the provider.
//!
//! # Why a test binary of its own
//!
//! A launch reads the user's registry and credential store from the process environment
//! (`XDG_CONFIG_HOME`, `MARION_CREDENTIAL_STORE`), exactly as a supervisor does. This binary sets
//! both once, to a fixture directory it writes itself — a `credentials.json` holding the fixture key
//! `sk-endpoint-test` and a `providers.toml` naming `canned-test` — so no test here can read the
//! user's real configuration, touch the Keychain, or reach a real provider. Putting these cells in
//! `harness_matrix` would hand that environment to its canned cells' children too.
//!
//! Cells are serialized: `providers.toml` points `canned-test` at one cell's canned server at a
//! time, since each server binds its own port.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use marion_core::contract::{Isolation, TaskId};
use marion_core::paths::ProjectDir;
use marion_supervisor::run::{Caller, Env, SpawnRequest, run_spawn};
use marion_testsupport::{fixture_repo, scratch};

/// The fixture key. The test writes it; nothing in marion ever mints it.
const KEY: &str = "sk-endpoint-test";

struct Fixture {
    config: PathBuf,
}

/// The one fixture config dir for this binary, with the environment pointed at it.
fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("marion-endpoint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let config = root.join("marion");
        std::fs::create_dir_all(&config).unwrap();
        write_owner_only(
            &config.join("credentials.json"),
            &format!("{{\"providers\": {{\"canned-test\": \"{KEY}\"}}}}\n"),
        );
        // SAFETY: set once, inside `get_or_init`, before any test in this binary has read the
        // environment or started a process — every test's first act is to call this.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &root);
            std::env::set_var("MARION_CREDENTIAL_STORE", "file");
        }
        Fixture { config }
    })
}

fn write_owner_only(path: &Path, body: &str) {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    f.write_all(body.as_bytes()).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// Hold the cell lock and point the fixture providers at `base_url`: `canned-test` serving all
/// four wires, `canned-chat` serving Chat Completions alone, `canned-nokey` with no stored key.
fn providers_at(base_url: &str) -> MutexGuard<'static, ()> {
    static CELLS: Mutex<()> = Mutex::new(());
    let guard = CELLS.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fixture();
    std::fs::write(
        fx.config.join("providers.toml"),
        format!(
            "[providers.canned-test]\nbase_url = \"{base_url}\"\n\
             wires = [\"anthropic\", \"openai-chat\", \"openai-responses\", \"gemini\"]\n\n\
             [providers.canned-chat]\nbase_url = \"{base_url}\"\nwires = [\"openai-chat\"]\n\n\
             [providers.canned-nokey]\nbase_url = \"{base_url}\"\nwires = [\"openai-chat\", \
             \"openai-responses\"]\n"
        ),
    )
    .unwrap();
    guard
}

struct Tree {
    _root: marion_testsupport::Scratch,
    repo: PathBuf,
    state: PathBuf,
    env: Env,
}

fn tree(tag: &str, base_url: Option<String>) -> Tree {
    let root = scratch(&format!("endpoint-{tag}"));
    let repo = fixture_repo(&root);
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let env = Env {
        project_dir: ProjectDir::new(&state, &repo),
        project_root: repo.clone(),
        state: state.clone(),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url,
        auth: marion_harness::Auth::Canned,
    };
    Tree {
        _root: root,
        repo,
        state,
        env,
    }
}

fn request(t: &Tree, agent_type: &str, model: &str) -> SpawnRequest {
    SpawnRequest {
        agent_type: agent_type.into(),
        prompt: "Report back through marion.".into(),
        repo: t.repo.clone(),
        acceptance_criteria: vec![],
        verification: vec![],
        writable_scope: vec!["src/**".into()],
        timeout_secs: 60,
        model: Some(model.into()),
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
    }
}

/// A refusal comes before the node exists: no journal record at all, no contract.
fn assert_refused_before_the_node_existed(t: &Tree, err: &str, needles: &[&str]) {
    for n in needles {
        assert!(err.contains(n), "the refusal must name {n:?}: {err}");
    }
    assert!(!err.contains(KEY), "{err}");
    let journal = marion_supervisor::journal::read(&t.env.project_dir).unwrap();
    assert_eq!(journal.records, 0, "a refused endpoint journals nothing");
    assert!(
        marion_testsupport::persisted_contracts(&t.state)
            .unwrap_or_default()
            .is_empty()
    );
}

fn spawn_err(t: &Tree, agent_type: &str, model: &str) -> String {
    run_spawn(
        &t.env,
        &request(t, agent_type, model),
        &TaskId(format!("endpoint-{agent_type}")),
        &Caller::root(
            "root",
            marion_core::agent_type::builtin("claude").expect("the root type resolves"),
        ),
    )
    .expect_err("an unusable endpoint is refused")
    .to_string()
}

#[test]
fn a_provider_with_no_stored_key_is_refused_by_name_with_the_login_command() {
    let t = tree("nokey", Some("http://127.0.0.1:9/v1".into()));
    let _cells = providers_at("http://127.0.0.1:9/v1");
    let err = spawn_err(&t, "codex-impl", "canned-nokey:some-model");
    assert_refused_before_the_node_existed(&t, &err, &["marion login canned-nokey"]);
}

#[test]
fn a_provider_sharing_no_wire_with_the_harness_is_refused_naming_both() {
    let t = tree("nowire", Some("http://127.0.0.1:9/v1".into()));
    let _cells = providers_at("http://127.0.0.1:9/v1");
    let err = spawn_err(&t, "codex-impl", "canned-chat:some-model");
    assert_refused_before_the_node_existed(
        &t,
        &err,
        &["codex", "openai-responses", "canned-chat", "openai-chat"],
    );
}

#[test]
fn an_acp_type_naming_a_provider_is_refused_because_acp_has_no_endpoint_wire() {
    let t = tree("acp", Some("http://127.0.0.1:9/v1".into()));
    let _cells = providers_at("http://127.0.0.1:9/v1");
    let err = spawn_err(&t, "acp-opencode", "canned-test:some-model");
    assert_refused_before_the_node_existed(&t, &err, &["no endpoint wire"]);
}
