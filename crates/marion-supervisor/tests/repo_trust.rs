//! **A repository's own command runs only after the operator allows it** (`marion trust`).
//!
//! A cloned repository's `.marion/agents.toml` can bind an agent type to any command line with
//! `harness = "acp:<command>"`. These tests drive the two real launch paths — a child through
//! `run_spawn`, a root through `root::prepare` — against such a file, with a command that leaves a
//! marker when it runs, and prove: refused with the exact `marion trust allow` command and no
//! marker; run once allowed; refused again after an edit; refused under a store others can write;
//! that a type naming only built-in rows needs no trust at all; and that a row widening its node
//! without naming a command (a prompt prefix) needs the same trust.
//!
//! One `#[test]`, sequential: the trust store is found through `$XDG_DATA_HOME`, which this binary
//! sets once before any thread or process exists, and the permission case changes that store.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marion_core::contract::{Isolation, TaskId};
use marion_core::paths::ProjectDir;
use marion_supervisor::root::{RootError, RootSpec, prepare};
use marion_supervisor::run::{AGENT_TYPES_FILE, Caller, Env, SpawnRequest, run_spawn};
use marion_supervisor::spawn::SpawnError;
use marion_supervisor::trust::TrustError;
use marion_supervisor::types_snapshot::TypesSnapshot;
use marion_testsupport::{fixture_repo, scratch};

fn agents_toml(script: &Path) -> String {
    format!(
        "[[agent]]\nname = \"pwn\"\nharness = \"acp:/bin/sh {}\"\ndescription = \"d\"\n\n\
         [[agent]]\nname = \"reviewer\"\nharness = \"codex\"\ndescription = \"d\"\n\n\
         [[agent]]\nname = \"pilot\"\nharness = \"acp:copilot\"\ndescription = \"d\"\n",
        script.display()
    )
}

fn env_for(state: &Path, repo: &Path) -> Env {
    Env {
        project_dir: ProjectDir::new(state, repo),
        project_root: repo.to_path_buf(),
        state: state.to_path_buf(),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url: None,
        auth: marion_harness::Auth::Inherited,
    }
}

fn request(repo: &Path) -> SpawnRequest {
    SpawnRequest {
        review: None,
        race: None,
        agent_type: "pwn".into(),
        prompt: "hello".into(),
        repo: repo.to_path_buf(),
        acceptance_criteria: vec![],
        writable_scope: vec!["**".into()],
        timeout_secs: 20,
        model: None,
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
        verification: vec![],
        profile: None,
    }
}

fn root_spec(repo: &Path, state: &Path) -> RootSpec {
    RootSpec {
        wider_children: false,
        agent_type: "pwn".into(),
        prompt: "hello".into(),
        native_launch: None,
        repo: repo.to_path_buf(),
        state: state.to_path_buf(),
        base_url: None,
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        model: None,
        auth: marion_harness::Auth::Inherited,
        no_change_record: true,
        pane: false,
        resume: None,
        bound_secs: 20,
        profile: None,
    }
}

fn spawn(repo: &Path, state: &Path, n: u32) -> Result<(), SpawnError> {
    let caller = Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    );
    run_spawn(
        &env_for(state, repo),
        &request(repo),
        &TaskId(format!("t-{n}")),
        &caller,
    )
    .map(drop)
}

/// Whether the marker exists, giving a process that might have been started a bounded chance to
/// write it: absence is read only after the bound, and any error but `NotFound` is a failure.
fn ran(marker: &Path, bound: Duration) -> bool {
    let until = Instant::now() + bound;
    loop {
        match std::fs::symlink_metadata(marker) {
            Ok(_) => return true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("reading {}: {e}", marker.display()),
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn allow(repo: &Path, store: &Path) -> String {
    let mut out = Vec::new();
    marion_supervisor::trust::run(&["allow".into()], repo, store.to_path_buf(), &mut out)
        .unwrap_or_else(|e| panic!("allow: {e:?}"));
    String::from_utf8(out).unwrap()
}

fn assert_untrusted(e: &SpawnError, file: &Path, edited: bool) {
    match e {
        SpawnError::Untrusted(TrustError::Untrusted {
            file: f,
            edited: ed,
            agent_type,
            ..
        }) => {
            assert_eq!(f, file);
            assert_eq!(*ed, edited);
            assert_eq!(agent_type, "pwn");
            assert!(
                e.to_string()
                    .ends_with(&format!("marion trust allow {}", file.display())),
                "{e}"
            );
        }
        other => panic!("expected an untrusted refusal, got {other:?}"),
    }
}

#[test]
fn a_repositorys_command_runs_only_while_its_exact_bytes_are_allowed() {
    let root = scratch("repo-trust");
    let data = root.join("data");
    // SAFETY: the one test in this binary, before it starts any thread or process.
    unsafe { std::env::set_var("XDG_DATA_HOME", &data) };
    let store = data.join("marion/trusted.toml");
    let repo = fixture_repo(&root);
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let marker = root.join("pwned");
    let script = root.join("pwn.sh");
    std::fs::write(&script, format!("touch '{}'\n", marker.display())).unwrap();
    let file = repo.join(AGENT_TYPES_FILE);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let text = agents_toml(&script);
    std::fs::write(&file, &text).unwrap();
    let file = file.canonicalize().unwrap();

    // Untrusted: both launch paths refuse before any process, naming the allow command.
    let e = spawn(&repo, &state, 1).unwrap_err();
    assert_untrusted(&e, &file, false);
    match prepare(&root_spec(&repo, &state)) {
        Err(RootError::Run(e)) => assert_untrusted(&e, &file, false),
        Err(other) => panic!("expected an untrusted refusal, got {other:?}"),
        Ok(_) => panic!("an untrusted root was prepared"),
    }
    assert!(
        !ran(&marker, Duration::from_millis(500)),
        "the command ran untrusted"
    );

    // Built-in rows in the same untrusted file need no trust.
    for name in ["reviewer", "pilot", "codex", "acp-copilot"] {
        TypesSnapshot::take(&repo, None)
            .and_then(|s| s.launch_type(name))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }

    // Allowed: shown, recorded, and the command runs.
    let shown = allow(&repo, &store);
    assert!(shown.contains("program: /bin/sh"), "{shown}");
    assert!(shown.contains(&script.display().to_string()), "{shown}");
    assert_eq!(
        std::fs::metadata(&store).unwrap().permissions().mode() & 0o777,
        0o600
    );
    prepare(&root_spec(&repo, &state)).expect("an allowed root prepares");
    let _ = spawn(&repo, &state, 2);
    assert!(
        ran(&marker, Duration::from_secs(10)),
        "the allowed command did not run"
    );
    std::fs::remove_file(&marker).unwrap();

    // Edited: trust is by content, so any change revokes it.
    std::fs::write(&file, format!("{text}\n# edited\n")).unwrap();
    assert_untrusted(&spawn(&repo, &state, 3).unwrap_err(), &file, true);
    assert!(
        !ran(&marker, Duration::from_millis(500)),
        "the edited file's command ran"
    );

    // Re-allowed, then a store others can write: refused, and nothing runs.
    allow(&repo, &store);
    std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o666)).unwrap();
    match spawn(&repo, &state, 4).unwrap_err() {
        SpawnError::Untrusted(TrustError::UnsafeStore { .. }) => {}
        other => panic!("expected an unsafe-store refusal, got {other:?}"),
    }
    assert!(
        !ran(&marker, Duration::from_millis(500)),
        "ran under a writable store"
    );
    std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o600)).unwrap();

    // A row naming no command but widening its node (here a prompt prefix) needs the same trust,
    // and once allowed the contract records the prompt the child actually saw, prefix included.
    let prefixed = fixture_repo(&root.join("prefixed"));
    let file = prefixed.join(AGENT_TYPES_FILE);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        "[[agent]]\nname = \"reviewer\"\nharness = \"opencode\"\ndescription = \"r\"\n\
         model = \"marion/default\"\nprompt_prefix = \"Review only.\\n\\n\"\n",
    )
    .unwrap();
    match launch_type(&prefixed, "reviewer") {
        Err(e @ SpawnError::Untrusted(TrustError::Untrusted { .. })) => {
            assert!(e.to_string().contains("prompt_prefix = "), "{e}")
        }
        other => panic!("expected an untrusted refusal, got {other:?}"),
    }
    let shown = allow(&prefixed, &store);
    assert!(shown.contains("sets:    prompt_prefix = "), "{shown}");
    launch_type(&prefixed, "reviewer").expect("an allowed widening row resolves");
    // opencode, a `LaunchOnly` row: its prompt rides argv, so a dead endpoint and a bridge that
    // does not exist still end in a contract.
    if marion_testsupport::harness_available("opencode") {
        let contract = run_spawn(
            &env_for(&state, &prefixed),
            &SpawnRequest {
                agent_type: "reviewer".into(),
                prompt: "do the task".into(),
                repo: prefixed.clone(),
                timeout_secs: 1,
                ..request(&prefixed)
            },
            &TaskId("prefixed".into()),
            &Caller::root(
                "root",
                marion_core::agent_type::builtin("claude").expect("the root type resolves"),
            ),
        )
        .expect("an opencode child against a dead endpoint still ends in a contract");
        assert_eq!(
            contract.instructions.value,
            format!(
                "Review only.\n\ndo the task\n\n{}",
                marion_supervisor::bridge::REPORT_INSTRUCTION
            )
        );
    }
}
