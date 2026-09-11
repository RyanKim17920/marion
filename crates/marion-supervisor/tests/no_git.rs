//! **What marion does in a directory that is not a git repository.**
//!
//! This path had no tests at all, and it is the one the operator asked about: *"if you're not
//! working in a git repo it's bad as well… why are we even trying to do this?"* §2 keys a project
//! on *"the git common-dir, falling back to cwd"*, and `socket.rs` names the rejected alternative
//! outright — *"the alternative is refusing to run outside git."* But §3.1 makes `shared-cwd` the
//! **default** for `isolation`, and until `fc80ec2` marion refused it by name and served only
//! `worktree`, the one value that hard-requires a repository. So marion refused its own default,
//! and a `spawn` outside a repository died several steps in on git's own stderr.
//!
//! Four measurements, one per thing that used to be untested:
//!
//! 1. a `shared-cwd` child **runs** in a directory with no repository, its contract carries
//!    [`Workspace::SharedCwd`], and it says the scope was not checked rather than that it passed;
//! 2. a `worktree` child there is **refused in marion's voice**, before anything irreversible,
//!    naming the command, the directory and both ways out;
//! 3. a second write-capable child in one cwd is refused **naming the holder** (§6.6);
//! 4. `marion run` works end to end with **no git anywhere in the run** — a real root, a real
//!    child, in a directory `git rev-parse` reports nothing about.
//!
//! **Where these directories live matters.** `scratch()` roots under `/tmp`, which is outside any
//! repository; a fixture built under `target/` would sit inside marion's own checkout and
//! `--git-common-dir` would find *that*, so every test here would silently measure the wrong thing
//! while passing. [`plain_dir`] asserts the property rather than assuming it.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test no_git
//! ```
//!
//! Tests 1 and 4 need a real `codex` (and 4 a real `claude`) on `PATH`, at the pinned versions.
//! They do **not** skip when it is missing: a criterion that quietly passes on a machine that
//! cannot run it is worth less than no criterion.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use marion_core::contract::{AgentId, Isolation, TaskContract, TaskId, Workspace};
use marion_core::paths::ProjectDir;
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::run::{Caller, Env, SpawnRequest, run_bounded, run_spawn};
use marion_supervisor::spawn::CwdClaim;
use marion_testsupport::{on_path, pinned_version, scratch};

const CHILD_TIMEOUT_SECS: u64 = 60;
const NARRATIVE: &str = "Edited the shared cwd and reported back.";

/// The file the canned child writes. Inside `src/**`, which is the `writable_scope` every request
/// here asks for — so if the scope check *did* run it would pass, and a `scope_enforced: true`
/// could not be distinguished from an honest one by looking at `scope_violations` alone. That is
/// exactly why §6.7 splits the two fields, and why test 1 asserts on the flag.
const CHILD_PATCH: &str = "*** Begin Patch\n*** Add File: src/marion_shared.txt\n\
                           +written by the canned codex child, in the caller's own directory\n\
                           *** End Patch";

/// A directory with a `src/` in it and **no repository anywhere above it**.
///
/// The second half is asserted, not assumed. `git rev-parse --git-common-dir` walks *upwards*, so
/// "I did not run `git init` here" is not the same claim as "this is not in a repository", and the
/// difference is the whole subject of this file.
fn plain_dir(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/keep.txt"), "keep\n").unwrap();
    let out = Command::new("git")
        .current_dir(&dir)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .expect("git runs");
    assert!(
        !out.status.success(),
        "{} is inside a git repository, so nothing in this file would measure the no-git path — \
         git answered: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stdout).trim()
    );
    dir
}

/// The pieces `run_spawn` takes, over a directory the caller chooses.
struct Fixture {
    cwd: PathBuf,
    env: Env,
    /// Held, not dropped: dropping the server closes the port the child talks to.
    _server: CannedServer,
}

fn fixture(root: &Path, cwd: PathBuf) -> Fixture {
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: root.join("provider-requests.jsonl"),
        script: Script {
            child_narrative: NARRATIVE.into(),
            child_patch: CHILD_PATCH.into(),
            ..Script::default()
        },
    })
    .expect("the canned provider binds");
    let env = Env {
        // §2's fallback, exercised for real: with no git common dir, the project key *is* this
        // directory. `c71f48a` is what made that derivation single-sourced.
        project_dir: ProjectDir::new(&state, &cwd),
        state,
        project_root: cwd.clone(),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url: Some(server.base_url()),
        auth: marion_harness::Auth::Canned,
    };
    Fixture {
        cwd,
        env,
        _server: server,
    }
}

fn request(fx: &Fixture, isolation: Isolation) -> SpawnRequest {
    SpawnRequest {
        agent_type: "codex-impl".into(),
        prompt: "Edit the file under src/ and report back through marion.".into(),
        repo: fx.cwd.clone(),
        acceptance_criteria: vec!["a file under src/ was edited".into()],
        writable_scope: vec!["src/**".into()],
        timeout_secs: CHILD_TIMEOUT_SECS,
        model: None,
        isolation,
        allow_concurrent_writes: false,
        resume: None,
    }
}

/// A root caller: depth 0, the same thing `marion run` hands the bridge.
fn caller() -> Caller {
    Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    )
}

fn spawn(fx: &Fixture, task: &str, isolation: Isolation) -> Result<TaskContract, String> {
    run_spawn(
        &fx.env,
        &request(fx, isolation),
        &TaskId(task.into()),
        &caller(),
    )
    .map_err(|e| e.to_string())
}

// --- 1: shared-cwd outside a repository -----------------------------------------------------

/// **The default §3.1 states, in the place it was never able to run.**
///
/// Three things at once, because they are one claim: the child *runs*, it runs in the caller's own
/// directory rather than a worktree marion made, and the contract says the scope check did not
/// happen instead of claiming it passed.
///
/// The last one is the part that matters and the part a lazier implementation gets wrong. With no
/// repository there is no `base_commit`, so neither of §6.7's two diff routes exists; the old code
/// wrote `changed_paths(..).unwrap_or_default()` and left `scope_enforced: true`, which is a clean
/// bill of health issued by a check that never ran. §6.7 is explicit that the flag records whether
/// the check **ran**, not whether it passed, and §9's whole argument is that *"no check performed"*
/// must not serialize as a clean result.
///
/// `changed_paths` is asserted **empty**, not "whatever git said": there is no honest source for it
/// here, and a filesystem walk standing in for git would be a different measurement wearing the
/// same field name. Empty-and-unenforced is readable; populated-and-unenforced would invite a
/// reader to trust it.
#[test]
fn a_shared_cwd_child_runs_outside_a_repository_and_records_that_the_scope_was_not_checked() {
    assert!(
        on_path("codex"),
        "this measures a REAL codex child in a directory with no repository; put `codex` ({}) on \
         PATH",
        pinned_version("codex")
    );
    let root = scratch("nogit-shared");
    let cwd = plain_dir(&root, "plain");
    let fx = fixture(&root, cwd.clone());

    let contract = spawn(&fx, "shared-1", Isolation::SharedCwd)
        .unwrap_or_else(|e| panic!("a shared-cwd child must run outside a repository: {e}"));

    match &contract.workspace {
        Workspace::SharedCwd { path } => assert_eq!(
            path.canonicalize().unwrap(),
            cwd.canonicalize().unwrap(),
            "the workspace is the caller's own directory, not one marion made"
        ),
        other => panic!("expected Workspace::SharedCwd, got {other:?}"),
    }
    assert_eq!(
        contract.base_commit, None,
        "there is no commit to branch from, and a synthetic oid would be a commit id a reader \
         could look up, fail to find, and mistake for a pruned object rather than for an absence"
    );
    // Unwrapped rather than `if let`: a child that never reported has no completion at all, and
    // silently skipping these three assertions is how this test would go green against a run that
    // never happened.
    let comp = contract
        .completion
        .as_ref()
        .expect("the child reported, so there is a completion to judge");
    assert!(
        !comp.scope_enforced,
        "no repository means neither of §6.7's diff routes ran, and the flag records whether the \
         check ran — not whether it passed"
    );
    assert!(
        comp.changed_paths.is_empty(),
        "nothing may be fabricated for this field: git is §6.7's one authority for it, got {:?}",
        comp.changed_paths
    );
    assert!(
        comp.scope_violations.is_empty(),
        "a check that did not run found no violations; the absence is carried by the flag, not by \
         this list, got {:?}",
        comp.scope_violations
    );

    // marion did not quietly make the directory into a repository, and did not add a worktree to
    // one it did not make. §6.6 says marion never auto-merges, and creating a repository in
    // someone's directory is the larger uninvited act.
    assert!(
        !cwd.join(".git").exists(),
        "a shared-cwd run must not `git init` the operator's directory"
    );
    // The child really did the work, in the caller's own tree — which is what `shared-cwd` is for.
    assert_eq!(
        std::fs::read_to_string(cwd.join("src/marion_shared.txt")).ok(),
        Some("written by the canned codex child, in the caller's own directory\n".into()),
        "the child's write landed in the caller's directory"
    );
}

// --- 2: worktree outside a repository -------------------------------------------------------

/// **The refusal that replaces a leaked `fatal: not a git repository`.**
///
/// That message came out of `git`'s generic stderr passthrough inside `make_worktree`, and it named
/// neither the command marion ran nor the directory it ran it in — so an operator saw git's words
/// about a directory git did not mention, from a tool they did not invoke, and could not tell which
/// of `--repo`, their cwd, or marion itself was wrong.
///
/// The assertions are the four things that make it actionable, and each is a separate way for a
/// well-meaning rewording to make it useless again: the **directory**, the **command** whose answer
/// marion acted on, and **both** exits. Both exits, because which one is right is the operator's
/// call and not marion's — `git init` keeps the containment, `shared-cwd` keeps the tree as it is —
/// and guessing either would be a side effect nobody asked for.
#[test]
fn a_worktree_child_outside_a_repository_is_refused_in_marions_voice() {
    let root = scratch("nogit-wt");
    let cwd = plain_dir(&root, "plain");
    let fx = fixture(&root, cwd.clone());

    let msg = spawn(&fx, "wt-1", Isolation::Worktree)
        .expect_err("a worktree child cannot run where there is no repository to add one to");

    assert!(
        msg.contains(&cwd.display().to_string()),
        "the refusal names the directory, which is the thing the operator can look at: {msg}"
    );
    assert!(
        msg.contains("rev-parse --git-common-dir"),
        "and the command whose answer marion acted on, so \"marion thinks this is not a repo\" is \
         checkable rather than a claim: {msg}"
    );
    assert!(
        msg.contains("git init"),
        "and the exit that keeps the containment: {msg}"
    );
    assert!(
        msg.contains("shared-cwd"),
        "and the exit that keeps the tree as it is: {msg}"
    );
    assert!(
        msg.contains("Nothing was started"),
        "and says so, because the operator's next question is whether to clean up: {msg}"
    );

    // **Refused before the first irreversible thing a spawn does.** A worktree is that thing: a
    // failed spawn that already added one leaves a directory, a `.git/worktrees/` entry and a
    // branch behind. This is the assertion the message cannot make for itself.
    assert!(
        !cwd.join(".git").exists(),
        "nothing was created in the operator's directory"
    );
    let contracts = fx.env.project_dir.path().join("contracts");
    assert!(
        std::fs::read_dir(&contracts)
            .map(|d| d.count())
            .unwrap_or(0)
            == 0,
        "and no contract was written for a node that never started"
    );
}

// --- 3: §6.6's write-conflict refusal --------------------------------------------------------

/// **§6.6: at most one write-capable node per cwd, refused naming the holder.**
///
/// The rule was unreachable until `shared-cwd` was built — every child got its own worktree, so no
/// cwd was ever occupied twice, and §11 item 23 recorded it as a refusal that is not in code.
///
/// **The holder is taken directly rather than by racing two spawns**, and that is a deliberate
/// weakening. A real race would need the first child to still be live when the second is attempted,
/// which is a timing assumption, and a test that passes only when the sleep is long enough is a
/// test that will one day pass for the wrong reason. What is asserted instead is the whole of what
/// the guard does at the spawn edge: an occupied cwd refuses the next write-capable child, by name.
/// The atomicity of check-and-claim — the property a race would be probing — is asserted directly
/// in `spawn.rs`'s own unit tests, where it can be stated rather than provoked.
#[test]
fn a_second_write_capable_child_in_one_cwd_is_refused_naming_the_holder() {
    let root = scratch("nogit-occ");
    let cwd = plain_dir(&root, "plain");
    let fx = fixture(&root, cwd.clone());

    let holder = AgentId("impl-auth-7f21".into());
    let claim = CwdClaim::claim(&cwd, &holder).expect("an empty cwd is free");

    let msg = spawn(&fx, "occupied-1", Isolation::SharedCwd)
        .expect_err("a second write-capable node in one cwd is refused (§6.6)");

    assert!(
        msg.contains("impl-auth-7f21"),
        "the refusal names the holder, which is the only part a caller can act on — \"this \
         directory is busy\" leaves them guessing which of their children to wait for: {msg}"
    );
    assert!(
        msg.contains(&cwd.canonicalize().unwrap().display().to_string()),
        "and the directory: {msg}"
    );
    assert!(
        msg.contains("Nothing was started"),
        "and that nothing was started, so the caller does not go looking for a node: {msg}"
    );
    for way_out in ["Wait for", "worktree", "allow_concurrent_writes"] {
        assert!(
            msg.contains(way_out),
            "and all three ways past it — waiting, a private tree, or taking the risk knowingly — \
             missing {way_out:?}: {msg}"
        );
    }

    // **Released when the holder goes**, which is what makes the guard a guard rather than a
    // permanent block. A leaked claim is worse than no guard at all: it would refuse every future
    // spawn into that directory for the life of the supervisor, naming a node that has exited, and
    // an operator could not clear it without restarting.
    drop(claim);
    assert!(
        on_path("codex"),
        "the release is proved by a real spawn succeeding; put `codex` ({}) on PATH",
        pinned_version("codex")
    );
    spawn(&fx, "occupied-2", Isolation::SharedCwd).expect("the cwd is free once the holder drops");
}

/// **The escape hatch is the one thing that suppresses the refusal, and nothing else does.**
///
/// Asserted beside the refusal rather than in the accepting table in `mcp.rs`, because that table
/// only proves the value is not rejected at the schema edge — it never reaches the guard. This is
/// the assertion that `allow_concurrent_writes: true` actually gets a child past an occupied cwd.
#[test]
fn allow_concurrent_writes_is_what_gets_a_second_writer_into_an_occupied_cwd() {
    assert!(
        on_path("codex"),
        "this measures a REAL second child entering an occupied cwd; put `codex` ({}) on PATH",
        pinned_version("codex")
    );
    let root = scratch("nogit-acw");
    let cwd = plain_dir(&root, "plain");
    let fx = fixture(&root, cwd.clone());

    let _claim = CwdClaim::claim(&cwd, &AgentId("holder".into())).expect("an empty cwd is free");

    let mut req = request(&fx, Isolation::SharedCwd);
    req.allow_concurrent_writes = true;
    run_spawn(&fx.env, &req, &TaskId("acw-1".into()), &caller())
        .expect("the escape hatch lets a second writer in, deliberately (§5.4, §6.6)");
}

// --- 4: the whole thing, with no git anywhere ------------------------------------------------

/// **`marion run` end to end in a directory that is not a repository.**
///
/// The operator's question, answered by the binary rather than by a library call: a real `claude`
/// root, a real `codex` child, a directory `git rev-parse` reports nothing about, and no `git init`
/// anywhere. Everything below the binary is exercised for real — §2's cwd fallback for the project
/// key and the socket path, §9's change record finding no tree to snapshot, and the child's
/// `shared-cwd` workspace.
///
/// The root asks for `isolation: "shared-cwd"` explicitly. It has to: absence resolves to
/// `worktree` (see `contract::Isolation` for why silence must not remove containment), which here
/// is refused — correctly, and by the sentence test 2 pins. So this run also witnesses that the
/// refusal names a way out that *works*, rather than one that merely reads well.
#[test]
fn marion_run_works_end_to_end_with_no_git_anywhere() {
    for program in ["claude", "codex"] {
        assert!(
            on_path(program),
            "this is the whole no-git path against real binaries; put `{program}` ({}) on PATH",
            pinned_version(program)
        );
    }
    let root = scratch("nogit-e2e");
    let cwd = plain_dir(&root, "plain");
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let script = Script {
        root_tool_input: serde_json::json!({
            "agent_type": "codex-impl",
            "prompt": "Add the marker file under src/ and report back.",
            "acceptance_criteria": ["a file exists under src/ containing the marker"],
            "writable_scope": ["src/**"],
            // The whole point of the run: the child is told to use the caller's own directory,
            // because there is no repository for a worktree to be added to.
            "isolation": "shared-cwd",
        }),
        child_narrative: NARRATIVE.into(),
        child_patch: CHILD_PATCH.into(),
        ..Script::default()
    };
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: root.join("provider-requests.jsonl"),
        script,
    })
    .expect("the canned provider binds");

    let out = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_marion"))
            .args([
                "run",
                "claude",
                "--prompt",
                "Delegate the marker-file task to a codex child in this directory.",
                "--repo",
                &cwd.to_string_lossy(),
                "--state-dir",
                &state.to_string_lossy(),
                "--canned",
                "--base-url",
                &server.base_url(),
                "--timeout",
                "60",
            ])
            .current_dir(&cwd),
        RUN_BOUND,
    )
    .expect("marion run starts");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !out.timed_out,
        "marion run did not finish inside {RUN_BOUND:?}\nstderr:\n{stderr}"
    );
    assert_eq!(
        out.code,
        Some(0),
        "marion run must exit cleanly outside a repository — §2 falls back to cwd and \
         `socket.rs` names \"refusing to run outside git\" as the alternative it \
         rejected\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !cwd.join(".git").exists(),
        "and it did not make the operator's directory into a repository on the way"
    );
    assert!(
        cwd.join("src/marion_shared.txt").exists(),
        "the child's work landed in the caller's own directory\nstdout:\n{stdout}"
    );
}

/// Generous: the bound exists so a hung harness fails loudly instead of wedging the suite, not to
/// measure anything.
const RUN_BOUND: Duration = Duration::from_secs(300);
