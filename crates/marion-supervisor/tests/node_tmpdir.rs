//! **A node's own `TMPDIR`, on the harness that made it necessary.**
//!
//! opencode 1.18 is Bun-built, and Bun unpacks its embedded native libraries into `$TMPDIR` as
//! `.<16 hex>-00000000.dylib` on every launch and never deletes them: ~4.8 MB of `libfff_c` per
//! `opencode run`. Before each node got a temp dir of its own, every opencode child marion ran left
//! one in the operator's temp dir — 1,594 files, 7 GB, in one day of test runs.
//!
//! The claim is about the **parent's** `TMPDIR`, which a test process cannot safely repoint at a
//! scratch dir while other tests run beside it. So the outer test re-runs this binary on the inner
//! test alone, with `TMPDIR` set to a dir it made for the purpose, and inspects that dir after the
//! inner process — and the child it spawned — are gone.
//!
//! ```sh
//! cargo test -p marion-supervisor --test node_tmpdir
//! ```
//!
//! Needs a real `opencode` on `PATH` at the version `marion_testsupport::PINNED_HARNESSES` lists,
//! for `harness_matrix.rs`'s reason; nothing here logs in or reaches a real provider.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use marion_core::contract::{AgentId, ExitStatus, Isolation, TaskId};
use marion_core::paths::ProjectDir;
use marion_core::secret::Secret;
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::run::{Caller, Env, SpawnObserver, SpawnRequest, run_spawn_watched};
use marion_testsupport::{fixture_repo, harness_available, scratch, until_within};
use serde_json::json;

/// Set on the re-run: the inner test runs only when this names the dir the outer test checks.
const INNER: &str = "MARION_NODE_TMPDIR_INNER";

/// Hidden, and named by Bun's own scheme: `.` + 16 hex + `-` + an 8-digit counter + `.dylib`.
fn unpacked_libraries(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{} cannot be listed: {e}", dir.display()))
        .map(|entry| entry.expect("an entry reads").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| name.starts_with('.') && name.ends_with(".dylib"))
        .collect()
}

#[test]
fn an_opencode_child_leaves_no_unpacked_library_in_its_parents_tmpdir() {
    if !harness_available("opencode") {
        return;
    }
    let dir = scratch("node-tmpdir-outer");
    let parent_tmp = dir.join("parent-tmp");
    std::fs::create_dir(&parent_tmp).unwrap();

    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "the_inner_run_spawns_one_opencode_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("TMPDIR", &parent_tmp)
        .env(INNER, &parent_tmp)
        .output()
        .expect("this test binary re-runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "the inner run must have run and passed — its own assertion names what failed.\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        unpacked_libraries(&parent_tmp),
        Vec::<String>::new(),
        "the child unpacked into the temp dir marion runs in rather than its own"
    );
}

/// The inner half: one opencode child through `run_spawn`, watched while it runs. A no-op unless
/// the outer test started this process — in an ordinary run it passes having done nothing.
#[test]
fn the_inner_run_spawns_one_opencode_child() {
    let Some(parent_tmp) = std::env::var_os(INNER).map(PathBuf::from) else {
        return;
    };
    let dir = scratch("node-tmpdir-inner");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let project = ProjectDir::new(&state, &repo);
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: dir.join("provider-requests.jsonl"),
        script: Script {
            openai_report_tool: "marion_report".into(),
            openai_report_args: json!({ "narrative": "Reported from a private temp dir." }),
            ..Script::default()
        },
    })
    .expect("the canned provider binds");
    let env = Env {
        project_dir: project.clone(),
        project_root: repo.clone(),
        state: state.clone(),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url: Some(server.base_url()),
        auth: marion_harness::Auth::Canned,
    };
    let req = SpawnRequest {
        race: None,
        agent_type: "opencode".into(),
        prompt: "Report back through marion.".into(),
        repo: repo.clone(),
        acceptance_criteria: vec![],
        verification: vec![],
        writable_scope: vec!["src/**".into()],
        timeout_secs: 60,
        model: Some("marion/canned-1".into()),
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
        profile: None,
        review: None,
    };
    let caller = Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    );
    let watcher = TmpWatcher {
        project: project.clone(),
        seen: Arc::new(AtomicBool::new(false)),
        agent: Mutex::new(None),
    };

    let contract = run_spawn_watched(&env, &req, &TaskId("node-tmpdir".into()), &caller, &watcher)
        .expect("the child runs");

    let status = contract.completion.as_ref().map(|c| c.status);
    assert_eq!(status, Some(ExitStatus::Ok), "{contract:#?}");
    let agent = watcher
        .agent
        .lock()
        .unwrap()
        .clone()
        .expect("a process was started");
    assert!(
        watcher.seen.load(Ordering::SeqCst),
        "opencode unpacked nothing into the node's temp dir while it ran — either it no longer \
         does (and this test proves nothing) or it was not given that dir"
    );
    assert!(
        !project.agent(&agent).tmp_dir().exists(),
        "the node's temp dir is removed when its process is reaped"
    );
    assert_eq!(
        unpacked_libraries(&parent_tmp),
        Vec::<String>::new(),
        "nothing reached the supervisor's own temp dir"
    );
}

/// Watches the node's `<agent-dir>/tmp` from the instant its process exists until a Bun library
/// shows up there. Test-only polling: nothing else can observe the dir during the run.
struct TmpWatcher {
    project: ProjectDir,
    seen: Arc<AtomicBool>,
    agent: Mutex<Option<AgentId>>,
}

impl SpawnObserver for TmpWatcher {
    fn identified(&self, _: &AgentId) -> Option<Secret> {
        None
    }

    fn started(&self, agent_id: &AgentId, _: i32) {
        *self.agent.lock().unwrap() = Some(agent_id.clone());
        let tmp = self.project.agent(agent_id).tmp_dir();
        let seen = Arc::clone(&self.seen);
        // Detached: it ends on the first sighting or at its bound, whichever comes first.
        std::thread::spawn(move || {
            let found = until_within(Duration::from_secs(60), Duration::from_millis(20), || {
                std::fs::read_dir(&tmp).is_ok_and(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .any(|e| e.file_name().to_string_lossy().ends_with(".dylib"))
                })
            });
            seen.store(found, Ordering::SeqCst);
        });
    }
}
