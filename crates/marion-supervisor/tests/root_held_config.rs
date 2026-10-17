//! **A root's later launch names the repository config it held back**, in a repository the
//! operator trusted: a config the root wrote into its checkout after its first launch does not
//! load on the resume, and the resume's node carries the paths for its `Spawned` to journal.
//!
//! Its own binary because the trust store is found through `$XDG_DATA_HOME`, which this binary
//! sets once before any thread or process exists.

use std::path::PathBuf;

use marion_supervisor::root::{RootResume, RootSpec, prepare};

#[test]
fn a_resumed_root_holds_back_and_names_the_config_it_planted() {
    let root = marion_testsupport::scratch("root-held-config");
    let data = root.join("data");
    // SAFETY: the one test in this binary, before it starts any thread or process.
    unsafe { std::env::set_var("XDG_DATA_HOME", &data) };
    let repo = marion_testsupport::fixture_repo(&root);
    let store = data.join("marion/trusted.toml");
    marion_supervisor::trust::run(
        &[
            "allow".into(),
            "--repo".into(),
            repo.to_string_lossy().into(),
        ],
        &repo,
        store,
        &mut Vec::new(),
    )
    .expect("the repository is trusted");
    let spec = |resume: Option<RootResume>| RootSpec {
        os_sandbox: true,
        wider_children: false,
        budget: None,
        agent_type: "claude-orchestrator".into(),
        prompt: "delegate it".into(),
        native_launch: None,
        repo: repo.clone(),
        state: root.join("state"),
        base_url: Some("http://127.0.0.1:8099/v1".into()),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        model: marion_core::agent_type::builtin("claude-orchestrator")
            .unwrap()
            .model
            .clone(),
        auth: marion_harness::Auth::Canned,
        no_change_record: false,
        pane: false,
        resume,
        bound_secs: marion_core::agent_type::DEFAULT_TIMEOUT_SECS,
        profile: None,
    };
    let first = prepare(&spec(None)).expect("the root compiles");
    assert!(
        first.repo_config_held.is_empty(),
        "a first launch holds nothing back"
    );

    std::fs::create_dir_all(repo.join(".codex")).unwrap();
    std::fs::write(
        repo.join(".codex/config.toml"),
        "[mcp_servers.x]\ncommand = \"x\"\n",
    )
    .unwrap();
    let tree = marion_core::registry::replay(&std::fs::read(first.project.journal()).unwrap());
    let trust = tree
        .get(&first.agent_id)
        .and_then(|n| n.intent.clone())
        .expect("the first launch's intent")
        .trust;
    let resumed = prepare(&spec(Some(RootResume {
        agent_id: first.agent_id.clone(),
        session: "s1".into(),
        usage: None,
        trust,
    })))
    .expect("the resume compiles");
    assert_eq!(
        resumed.repo_config_held.paths,
        vec![".codex/config.toml".to_string()],
        "the planted config, named for the resume's Spawned"
    );
}
