//! **Many `marion` processes at once, in different projects** (design §2, §4.3, §5.7).
//!
//! §2 keys both the supervisor socket and the state directory on the project root — git's common
//! dir — and §5.7 makes the start race idempotent *per project*. Neither sentence is about more
//! than one project, and the isolation that follows from them has never been asserted end to end:
//! `socket.rs`'s own tests race sixteen processes onto **one** socket, which is the opposite
//! question. What is untested is the ordinary case an operator actually creates — several
//! checkouts, several terminals, several roots running at the same time.
//!
//! # Why this drives real processes and not threads
//!
//! The property is cross-**process**: separate `marion` binaries, separate detached supervisors,
//! separate journals opened `O_APPEND` by separate pids. A threaded version would share one address
//! space, one `journal::this_writer()` `OnceLock` and one `spawn::REPO_WRITE` mutex — every
//! coordination mechanism whose *absence* between processes is the thing in doubt. So each run is
//! `Command::spawn` of `CARGO_BIN_EXE_marion`; the test's own threads exist only to wait on them.
//!
//! # The barrier, and why it is not a sleep
//!
//! The strongest assertion here is a count of **live supervisors while every root is running**, and
//! a count is only meaningful if all of them are simultaneously up. That is arranged structurally:
//! each stub harness announces itself by creating a file and then blocks until the test creates
//! `go`. The test polls for all `N` announcements, measures `ps` with every root parked, and only
//! then releases them. Nothing here waits a fixed interval and hopes; the ordering is the
//! mechanism, per this repo's rule about `socket::acquire` and darwin's descriptor visibility.
//!
//! # Two tests, and only one of them is good news
//!
//! [`concurrent_runs_in_distinct_projects_never_share_a_supervisor_a_journal_or_a_record`] pins
//! what **works**: different projects do not interfere. It is built so that a marion which keyed
//! *anything* per-machine instead of per-project goes red three independent ways — the supervisor
//! census would be one rather than `N`, the state directory would hold one project dir rather than
//! `N`, and the surviving journal would carry `N` `RootChanged` records rather than one. Each was
//! watched red on its own.
//!
//! [`two_concurrent_roots_in_one_repository_each_record_the_others_work_as_their_own`] pins what
//! does **not** work, and asserts marion's *current* behaviour rather than its desired one: inside
//! one repository, two overlapping roots each journal the other's writes as theirs. It is a
//! recorded limitation in the sense
//! `detached_supervisor.rs::a_journal_the_registry_cannot_parse_freezes_the_exit_predicate_and_says_so_by_name`
//! established — green today, and whoever fixes the limitation has to delete it deliberately. Its
//! own negative control is the sequential case: run the same two roots one after the other and each
//! record names only its own file, which is what proves the concurrent assertion is measuring
//! overlap and not something that was always true.
//!
//! Design §11 item 31 carries the same-repo limitations in full, including the git-level one
//! (S17, `tests/fixtures/s17/README.md`) that no test here asserts because a test whose verdict is
//! *"a race occurred"* is a flake by construction.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use marion_core::journal::{RecordKind, decode};
use marion_core::paths::ProjectDir;
use marion_core::root_change::{RootChange, RootDelta};
use marion_supervisor::socket::project_root;
use marion_testsupport::fixture_repo;

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

const SIGKILL: i32 = 9;

/// How many projects run at once. Four, for `background_spawn.rs`'s reason — it is above the point
/// where a shared-by-accident resource shows up and below the point where the machine running the
/// suite is the thing being measured.
const PROJECTS: usize = 4;

/// A ceiling on the whole fleet, not a verdict. Every assertion below is a count or a set; this
/// only decides whether a wedged run fails the suite loudly or hangs it.
const FLEET_BOUND: Duration = Duration::from_secs(180);

/// One marion verb, called and answered, in codex's stream shape — without it the run is refused as
/// `BridgeNeverReached`. Copied deliberately rather than shared with `root_change_record.rs`: that
/// file's constant is part of *its* fixture, and a shared one would make either file's stream shape
/// changeable from the other.
const REACHED_THE_BRIDGE: &str = r#"echo '{"type":"item.completed","item":{"id":"item_0","type":"mcp_tool_call","server":"marion","tool":"spawn","arguments":{},"status":"completed"}}'"#;

/// A **short** state directory, shared by every project in one test.
///
/// Short for `socket.rs`'s reason: under `temp_dir()` on macOS the socket path overruns `sun_path`
/// and marion falls back to `/tmp/marion-<uid>/`, which is a directory this test cannot clean up
/// without touching other runs' files. Keeping the primary branch live also keeps the *interesting*
/// paths under one removable root.
///
/// **Shared, and that is the point.** Isolation between projects must come from §2's key, not from
/// the operator having handed each run a different `--state-dir`. A version of this test that gave
/// each project its own state dir would pass against a marion with no project keying at all.
struct Fleet {
    state: PathBuf,
    repos: Vec<PathBuf>,
    /// Where the stub harnesses announce themselves and where `go` is written.
    barrier: PathBuf,
    scratch: marion_testsupport::Scratch,
}

impl Fleet {
    /// `repos` distinct checkouts under one shared `--state-dir`.
    ///
    /// The count is a parameter because the two tests in this file differ in exactly it: four
    /// projects for the isolation claim, **one** for the same-repo limitation below. Everything
    /// else — the short state dir, the barrier, the cleanup — is the same bed, which is what makes
    /// the second test's failure attributable to the shared repository and to nothing else.
    fn new(tag: &str, repos: usize) -> Fleet {
        let scratch = marion_testsupport::scratch(tag);
        let state = PathBuf::from(format!("/tmp/mcp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        std::fs::create_dir_all(&state).expect("scratch state dir");
        let barrier = scratch.join("barrier");
        std::fs::create_dir_all(&barrier).expect("barrier dir");

        let repos = (0..repos)
            .map(|i| {
                let home = scratch.join(format!("p{i}"));
                std::fs::create_dir_all(&home).expect("project home");
                fixture_repo(&home)
            })
            .collect();

        Fleet {
            state,
            repos,
            barrier,
            scratch,
        }
    }

    /// This project's `<state>/<project-hash>`, derived the way marion derives it — through
    /// [`project_root`], so a test that disagreed with §2's key would be asserting about a
    /// directory marion never wrote.
    fn project_dir(&self, i: usize) -> ProjectDir {
        ProjectDir::new(&self.state, &project_root(&self.repos[i]))
    }

    /// Every **stage-3** supervisor whose argv names this fleet's state directory.
    ///
    /// Returned with the `--project-root` argument beside the pid, because the count alone cannot
    /// tell `N` supervisors for `N` projects from `N` supervisors for one project — and the second
    /// is a failure §5.7 forbids just as firmly as the first.
    fn supervisors(&self) -> Vec<(i32, String)> {
        let out = Command::new("ps")
            .args(["-A", "-o", "pid=,command="])
            .output()
            .expect("ps runs");
        let needle = self.state.display().to_string();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains(&needle) && l.contains("--detached"))
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let pid: i32 = it.next()?.parse().ok()?;
                let rest: Vec<&str> = it.collect();
                let root = rest
                    .iter()
                    .position(|a| *a == "--project-root")
                    .and_then(|p| rest.get(p + 1))
                    .map(|s| (*s).to_string())?;
                Some((pid, root))
            })
            .collect()
    }

    /// Block until every stub has announced itself, or panic naming how many did.
    ///
    /// A panic and not a `bool`: the census that follows is only meaningful with all of them up,
    /// and a test that quietly measured three roots out of four would report isolation it never
    /// observed.
    fn await_all_started(&self) {
        let deadline = Instant::now() + FLEET_BOUND;
        loop {
            let started = std::fs::read_dir(&self.barrier)
                .map(|d| {
                    d.flatten()
                        .filter(|e| e.file_name().to_string_lossy().ends_with(".started"))
                        .count()
                })
                .unwrap_or(0);
            if started == PROJECTS {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "only {started} of {PROJECTS} roots ever reached their harness; the census below \
                 would have measured a fleet that was never fully up"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn release(&self) {
        std::fs::write(self.barrier.join("go"), b"go").expect("release the fleet");
    }
}

impl Drop for Fleet {
    /// Leave no supervisor behind, whatever the test did or failed to do — `detached_supervisor.rs`'s
    /// reason. A leaked one would hold this fleet's state directory for §5.7's whole idle grace,
    /// and forever if a node in its journal is non-terminal.
    fn drop(&mut self) {
        for (pid, _) in self.supervisors() {
            // SAFETY: `kill` with a pid this process just read from `ps`.
            unsafe { kill(pid, SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&self.state);
    }
}

/// A `codex` stub with `body` as its whole behaviour, on a `PATH` `dir/bin`.
fn stub(dir: &Path, body: &str) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("stub bin dir");
    let program = bin.join("codex");
    std::fs::write(&program, format!("#!/bin/sh\n{body}\n")).expect("write stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("stub is executable");
    }
    bin
}

/// **Announce, park until the test releases the fleet, write one file, answer the bridge.**
///
/// The wait's bound is the stub's own, so a fleet that is never released fails as `N` timed-out
/// runs rather than as a hung suite.
fn barrier_body(i: usize, barrier: &Path) -> String {
    let b = barrier.display();
    format!(
        ": > '{b}/{i}.started'\n\
         n=0\n\
         while [ ! -f '{b}/go' ]; do\n\
         n=$((n+1))\n\
         if [ \"$n\" -gt 4000 ]; then break; fi\n\
         sleep 0.05\n\
         done\n\
         printf 'project {i} wrote this\\n' > agent-{i}.txt\n\
         {REACHED_THE_BRIDGE}\n\
         exit 0"
    )
}

/// **A two-phase rendezvous that makes the same-repo overlap total rather than likely.**
///
/// The obvious one phase — write, then wait for the other file — is not enough, and the bug it
/// hides is subtle. A root's `pre_tree` is taken in `root::prepare`, *before* its harness launches.
/// With a single phase the second `marion` could reach `prepare` after the first root had already
/// written, so its `pre_tree` would contain that file, its delta would correctly name only its own,
/// and the test would fail on a fast machine and pass on a slow one. Neither outcome would mean
/// anything.
///
/// So there are two barriers, and the first one is the load-bearing one:
///
/// 1. **Announce and wait for the other announcement.** A stub only runs after its own `prepare`,
///    so once both have announced, **both `pre_tree`s are taken** — and neither file exists yet.
/// 2. **Write, then wait for the other file.** Neither root can exit until both files exist, so
///    **both `post_tree`s contain both**.
///
/// Together those two make the intervals strictly overlapping by construction. No sleep decides
/// anything; the loops' bounds only turn a wedged pair into a loud failure. The announcements live
/// in the barrier directory and not the repository, because a marker written *inside* the measured
/// tree would show up in the very deltas under assertion.
/// # One script, two roles, and why it cannot be two scripts
///
/// Each run used to get its **own** stub, on its own `PATH`, exported to its own `marion run`. That
/// stopped working at §11 item 28 step 6 and the reason is worth stating where the bed is, because
/// it is a property of marion and not of this file: **the root's process environment is now the
/// supervisor's, not the client's.** One repository is one project is one supervisor (§2), the
/// supervisor is started by whichever `marion run` arrived first and inherits *that* process's
/// environment, and every root it launches afterwards is spawned from it — so the second run's
/// `PATH` reaches nothing, both roots exec the first run's `codex`, and both wait for a file
/// neither will write. See `tasks/todo.md`'s finding (d).
///
/// So the role has to travel on a channel the client still owns, and the **prompt** is one: a
/// `LaunchOnly` root's prompt rides its argv (§6.1 step 8), which is where the stub reads it. That
/// is not a workaround for the test's benefit — it is the same narrowing an operator has, and a
/// test that kept pretending otherwise would be measuring a bed marion no longer builds.
fn rendezvous_stub(barrier: &Path) -> String {
    let b = barrier.display();
    format!(
        "case \"$*\" in\n\
         *{ROLE_A}*) mine=a.txt; theirs=b.txt ;;\n\
         *{ROLE_B}*) mine=b.txt; theirs=a.txt ;;\n\
         *) echo 'this root was launched with neither role in its prompt' >&2; exit 3 ;;\n\
         esac\n\
         : > \"{b}/$mine.ready\"\n\
         n=0\n\
         while [ ! -f \"{b}/$theirs.ready\" ]; do\n\
         n=$((n+1))\n\
         if [ \"$n\" -gt 4000 ]; then break; fi\n\
         sleep 0.05\n\
         done\n\
         printf 'written by the root that owns %s\\n' \"$mine\" > \"$mine\"\n\
         n=0\n\
         while [ ! -f \"$theirs\" ]; do\n\
         n=$((n+1))\n\
         if [ \"$n\" -gt 4000 ]; then break; fi\n\
         sleep 0.05\n\
         done\n\
         {REACHED_THE_BRIDGE}\n\
         exit 0"
    )
}

/// The two roles, as strings that can appear in nothing else a root is handed.
const ROLE_A: &str = "MARION-CONC-ROLE-A-4e17";
const ROLE_B: &str = "MARION-CONC-ROLE-B-4e17";

/// One `marion run` against `repo`, started but **not** waited on.
///
/// `home` is only the process's cwd and where its stub lives; `repo` is what marion is pointed at.
/// Keeping them separate is what lets two runs share a repository while still having distinct
/// stubs — which works only where the two runs are also distinct *projects*, because the stub
/// travels on `PATH` and a root's `PATH` is its supervisor's. See [`rendezvous_stub`].
fn spawn_run(fleet: &Fleet, home: &Path, repo: &Path, body: &str) -> std::process::Child {
    std::fs::create_dir_all(home).expect("run home");
    let bin = stub(home, body);
    spawn_run_with(fleet, home, repo, &bin, "Delegate the task to a child.")
}

/// [`spawn_run`], for a bed that installed the stub itself and needs the prompt to say which run
/// this is. See [`rendezvous_stub`] for why the prompt is the channel.
fn spawn_run_with(
    fleet: &Fleet,
    home: &Path,
    repo: &Path,
    bin: &Path,
    prompt: &str,
) -> std::process::Child {
    std::fs::create_dir_all(home).expect("run home");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(env!("CARGO_BIN_EXE_marion"))
        .args([
            "run",
            "codex",
            "--prompt",
            prompt,
            "--repo",
            &repo.to_string_lossy(),
            "--state-dir",
            &fleet.state.to_string_lossy(),
            // Nothing listens there; the stub is the whole model side of the run.
            "--canned",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--timeout",
            "120",
        ])
        .env("PATH", path)
        .current_dir(home)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("marion run starts")
}

/// Every `RootChanged` in a journal, in file order.
fn root_changes(journal: &Path) -> Vec<marion_core::root_change::RootChanged> {
    let Ok(bytes) = std::fs::read(journal) else {
        return Vec::new();
    };
    bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(decode)
        .filter_map(|r| match r.kind {
            RecordKind::RootChanged(c) => Some(c),
            _ => None,
        })
        .collect()
}

/// Every `root-change.json` anywhere under `dir`, parsed.
///
/// A walk rather than a probe at a computed path, for `root_change_record.rs`'s reason: asserting a
/// record is where the test guessed passes if marion wrote it somewhere else entirely.
fn sidecars(dir: &Path) -> Vec<RootChange> {
    let mut out = Vec::new();
    fn walk(d: &Path, out: &mut Vec<RootChange>) {
        let Ok(entries) = std::fs::read_dir(d) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.file_name().is_some_and(|n| n == "root-change.json")
                && let Ok(bytes) = std::fs::read(&p)
                && let Ok(v) = serde_json::from_slice::<RootChange>(&bytes)
            {
                out.push(v);
            }
        }
    }
    walk(dir, &mut out);
    out
}

fn changed_paths(sidecar: &RootChange) -> Vec<PathBuf> {
    match &sidecar.working_tree_delta {
        RootDelta::Observed { changed_paths, .. } => changed_paths.clone(),
        other => panic!("marion did not measure this run: {other:?}"),
    }
}

/// **Four `marion run`s in four checkouts, at the same time, sharing one `--state-dir` and nothing
/// else.**
///
/// The four assertions are deliberately independent, because §2's key could break in four different
/// places and any one of them alone is a silent data-mixing bug:
///
/// 1. **The census.** With every root parked on the barrier, `ps` shows exactly `PROJECTS`
///    detached supervisors and their `--project-root` arguments are exactly the four project keys,
///    each once. This is the direct reading of §5.7's *"one supervisor per project"* — nothing
///    else here observes a supervisor while it is alive.
/// 2. **The state directory.** Four distinct `<state>/<project-hash>` directories, each holding a
///    journal. A hash over anything less specific than the canonical common dir collapses them.
/// 3. **No cross-talk.** Each journal carries **exactly one** `RootChanged`, and each project's
///    sidecar names exactly its own `agent-<i>.txt`. Two projects sharing a journal would put two
///    records in one file; two projects sharing a *repo* would put both files in one delta.
/// 4. **No leaks.** Every run exits 0 and no supervisor naming this state dir survives the fleet.
///
/// **Watched red, three ways, by making the isolation fail rather than by weakening the test.**
/// Pointing `--repo` at one shared fixture for all four runs (which is what a marion with no
/// project keying would amount to) yields `1` supervisor against `4`, `1` project dir against `4`,
/// and a journal with `4` `RootChanged` records against `1` — assertions 1, 2 and 3 each failing on
/// their own. Deleting the barrier release turns assertion 1 into the `await_all_started` panic,
/// which is the fourth way and the reason that helper panics rather than returning a count.
#[test]
fn concurrent_runs_in_distinct_projects_never_share_a_supervisor_a_journal_or_a_record() {
    let fleet = Fleet::new("conc-proj", PROJECTS);

    let children: Vec<std::process::Child> = (0..PROJECTS)
        .map(|i| {
            let home = fleet.scratch.join(format!("p{i}"));
            let repo = fleet.repos[i].clone();
            spawn_run(&fleet, &home, &repo, &barrier_body(i, &fleet.barrier))
        })
        .collect();

    // Every root is now inside its harness, and none of them can finish until this test says so.
    fleet.await_all_started();

    // --- 1. the census, taken with the whole fleet alive ------------------------------------
    let live = fleet.supervisors();
    let mut roots: Vec<String> = live.iter().map(|(_, r)| r.clone()).collect();
    roots.sort();
    let mut expected: Vec<String> = (0..PROJECTS)
        .map(|i| project_root(&fleet.repos[i]).display().to_string())
        .collect();
    expected.sort();
    assert_eq!(
        roots, expected,
        "§5.7 is one supervisor per project, not one per machine and not two per project; ps saw \
         {live:?}"
    );

    fleet.release();

    let outcomes: Vec<(Option<i32>, String)> = children
        .into_iter()
        .map(|c| {
            let out = c.wait_with_output().expect("marion run is waitable");
            (
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        })
        .collect();
    for (i, (code, stderr)) in outcomes.iter().enumerate() {
        assert_eq!(*code, Some(0), "project {i} did not succeed: {stderr}");
    }

    // --- 2. four project directories, four journals ------------------------------------------
    let dirs: Vec<PathBuf> = (0..PROJECTS)
        .map(|i| fleet.project_dir(i).path().to_path_buf())
        .collect();
    let mut distinct = dirs.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        PROJECTS,
        "four checkouts must hash to four state directories: {dirs:?}"
    );
    let on_disk: Vec<PathBuf> = std::fs::read_dir(&fleet.state)
        .expect("state dir readable")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(
        on_disk.len(),
        PROJECTS,
        "marion wrote a different number of project directories than it was asked for: {on_disk:?}"
    );

    // --- 3. one record each, and every record names only its own project's file ---------------
    for (i, dir) in dirs.iter().enumerate() {
        let journal = dir.join("journal.jsonl");
        let records = root_changes(&journal);
        assert_eq!(
            records.len(),
            1,
            "project {i}'s journal must carry exactly its own root's record — more means two \
             projects shared a journal, none means this project never got one: {journal:?}"
        );
        let found = sidecars(dir);
        assert_eq!(found.len(), 1, "project {i}: {found:?}");
        assert_eq!(
            changed_paths(&found[0]),
            vec![PathBuf::from(format!("agent-{i}.txt"))],
            "project {i}'s change record folded in work that was not its own"
        );
        assert_eq!(
            found[0].record(),
            records[0],
            "project {i}: the journal's half is derived from the sidecar's, so a difference means \
             they were assembled twice"
        );
    }

    // --- 4. nothing left running -------------------------------------------------------------
    let deadline = Instant::now() + FLEET_BOUND;
    loop {
        let left = fleet.supervisors();
        if left.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "supervisors outlived their runs: {left:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// **A RECORDED LIMITATION, asserted as it currently behaves: two concurrent roots in one
/// repository each claim the other's work.**
///
/// This test does not describe something marion does right. It pins something marion does *wrong*,
/// so that the day it is fixed the fix has to say so out loud, in the pattern
/// `detached_supervisor.rs::a_journal_the_registry_cannot_parse_freezes_the_exit_predicate_and_says_so_by_name`
/// established.
///
/// # What breaks, and why it is structural
///
/// A root runs in `RootSpec::repo` — the operator's live checkout — and not in a worktree
/// (`root_change.rs`'s header, and the decision recorded in `tasks/design-root-change-record.md`
/// that roots get `read` + `write` there). Its change record is a delta between two git tree
/// snapshots, `pre_tree` at prepare and `post_tree` at exit. A tree delta is a statement about a
/// *directory over an interval*; it carries no attribution and cannot be made to. So two roots
/// whose intervals overlap in one repository each measure a post-tree containing the other's
/// writes, and each journals them as its own.
///
/// `tasks/design-root-change-record.md` states this under *"what this does NOT solve"* — *"Two runs
/// produce overlapping deltas each attributing the other's work to itself. No locking proposed."*
/// Nothing in `root.rs` refuses a second root, and `marion run` does not consult the registry for
/// one. This test is that sentence made falsifiable.
///
/// # Why the assertion is exact rather than "at least one is wrong"
///
/// Both roots write, then each waits for the other's file before finishing ([`rendezvous_body`]).
/// So **both** post-trees contain **both** files by construction, and the correct assertion is the
/// strong one: each record names two paths where its root wrote one. A weaker assertion — *"some
/// record folded something"* — would also pass on a machine that happened to serialise the two
/// runs, which is precisely the reading that must not be allowed to look like success.
///
/// The record is not merely imprecise, it is **wrong in the direction that matters**: an auditor
/// reading either sidecar is told this agent edited a file it never touched. That is `8a69f22`'s
/// failure class with the sign flipped, which the design document names in the same breath.
///
/// # What would close it
///
/// Three options, none of them this test's to pick, and all three recorded as design §11 item 31:
///
/// 1. **Refuse the second root.** `marion run` replays the journal and declines when a root for
///    this project is `is_unresolved()`. Cheapest, and the design document calls it *"a stated
///    refusal … and a separate decision"* — it changes what marion is willing to do, not just what
///    it records.
/// 2. **Lock the repository for a root's lifetime.** Turns concurrent roots into queued ones. Costs
///    the operator the ability to run two agents on one checkout at all, which may be the feature.
/// 3. **Accept and label.** Keep the behaviour, and have the record say the interval overlapped
///    another root's — which needs the registry read of option 1 anyway, and still leaves
///    `changed_paths` unattributable.
///
/// **This test goes red under option 1 or 2 and must be deleted or rewritten by whoever picks one.**
/// That is the point of pinning it.
#[test]
fn two_concurrent_roots_in_one_repository_each_record_the_others_work_as_their_own() {
    let fleet = Fleet::new("conc-same-repo", 1);
    let repo = fleet.repos[0].clone();

    // **One stub for both roots**, because one repository is one supervisor and a root's `PATH` is
    // its supervisor's — see [`rendezvous_stub`]. The role travels in the prompt, which is the one
    // channel each client still owns.
    let bin = stub(
        &fleet.scratch.join("shared-bin"),
        &rendezvous_stub(&fleet.barrier),
    );
    let a = spawn_run_with(
        &fleet,
        &fleet.scratch.join("run-a"),
        &repo,
        &bin,
        &format!("{ROLE_A}: delegate the task to a child."),
    );
    let b = spawn_run_with(
        &fleet,
        &fleet.scratch.join("run-b"),
        &repo,
        &bin,
        &format!("{ROLE_B}: delegate the task to a child."),
    );

    for (name, child) in [("a", a), ("b", b)] {
        let out = child.wait_with_output().expect("marion run is waitable");
        assert_eq!(
            out.status.code(),
            Some(0),
            "run {name} did not succeed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // Both roots really did write, and each really did see the other's file — otherwise the
    // rendezvous timed out and what follows would be measuring two runs that never overlapped.
    assert!(repo.join("a.txt").is_file() && repo.join("b.txt").is_file());

    // One repository is one project, so one journal — this is the *correct* half of the behaviour
    // and it is asserted so that a change to §2's key shows up here too.
    let dir = fleet.project_dir(0).path().to_path_buf();
    let records = root_changes(&dir.join("journal.jsonl"));
    assert_eq!(
        records.len(),
        2,
        "two roots ran in one project, so one journal carries two records; O_APPEND at record \
         granularity is what makes that safe (`journal.rs`'s MAX_RECORD_BYTES)"
    );

    let found = sidecars(&dir);
    assert_eq!(found.len(), 2, "one sidecar per root: {found:?}");

    let both = vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")];
    for (i, sidecar) in found.iter().enumerate() {
        let mut paths = changed_paths(sidecar);
        paths.sort();
        assert_eq!(
            paths, both,
            "RECORDED LIMITATION, not a passing feature: root {i} wrote exactly one of these files \
             and its change record claims both. A tree delta is a statement about a directory over \
             an interval and cannot attribute; see this test's doc comment and design §11 item 31 \
             for the three things that would close it. If this assertion has started failing, \
             marion has stopped attributing another root's work — delete this test rather than \
             repairing it."
        );
    }
}
