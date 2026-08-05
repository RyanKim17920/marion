//! **The helpers marion's tests share, in one copy each.**
//!
//! # Why this is a crate and not a `tests/common/mod.rs`
//!
//! Rust compiles every file in `tests/` as its own crate, so integration tests cannot share code
//! without a mechanism, and there are two candidates. A `tests/common/mod.rs` reaches the eleven
//! integration files and **nothing else** — but four of the `Scratch` guards this crate replaces
//! live in `#[cfg(test)] mod tests` inside `src/journal.rs`, `src/run.rs` and `src/duplex.rs`, and
//! two more constructors in `src/root.rs` and `src/bin/marion.rs`. A module compiled into the
//! integration-test targets is invisible to those, so `tests/common` would leave a third of the
//! duplication in place while looking like it had solved the problem. A dev-dependency is visible
//! to a crate's own `#[cfg(test)]` code as well, which is the whole of the argument.
//!
//! It is also the choice this workspace has already made once, for the same reason: `marion-provider`
//! is a member crate whose only consumers are tests, wired in as marion-supervisor's single
//! `[dev-dependencies]` edge. Following that is not inventing structure. `#[path = …]`, by
//! contrast, appears nowhere in this repository.
//!
//! # Why a shared copy at all, when each copy was already documented
//!
//! Not tidiness. **The copies did not merely multiply — the strong version failed to propagate**,
//! and the drift always ran the same way, toward the weaker assertion:
//!
//! - [`survivors`] existed five times in two strengths. Three copies parsed the pid off each `ps`
//!   line and panicked when they could not; two returned the line as a string and silently dropped
//!   the pid. The comment explaining why that matters — *a survivor this test cannot name* — was
//!   only ever in the three strong copies. Two files had a leak check that could under-report.
//! - [`Scratch`] existed eleven times, and beside it four constructors that returned a bare
//!   `PathBuf` with no guard at all. Those four leaked their directory on every failing run, which
//!   is precisely the asymmetry the guard was introduced to remove.
//! - [`persisted_contracts`] existed five times in three different failure semantics.
//!
//! A per-file copy that has drifted is worse than either sharing or duplicating on purpose. So each
//! helper here is the **strongest** variant that existed, and the reasoning that made it the
//! strongest is kept with it, where the next person will read it before weakening it.
//!
//! # This crate must never be a normal dependency
//!
//! Same rule as `marion-provider`, and the same reason: nothing here belongs inside the shipped
//! `marion` command. It kills processes by pid, removes directories on drop, and shells out to
//! `git` and `ps`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

// --- processes ----------------------------------------------------------------------------------

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// `SIGKILL`. Not `SIGTERM`: a test cleaning up after a leak has already established that the
/// process ignored whatever polite signal its supervisor sent it.
const SIGKILL: i32 = 9;
/// Signal 0 — the null signal, which checks for the process's existence and delivers nothing.
const SIGNULL: i32 = 0;
/// "No such process": the *only* errno from `kill(pid, 0)` that means the pid is gone.
const ESRCH: i32 = 3;

/// Is a process with this pid still alive?
///
/// `kill(pid, 0)` rather than a `ps` scan: it answers about one pid without parsing anything, which
/// is what a test polling for a death wants.
///
/// **A non-zero return is not a death.** `kill(pid, 0)` fails with `EPERM` for a process that
/// exists and is not ours, and treating that as gone is backwards in exactly the direction a leak
/// check cannot afford: a survivor that changed uid — or any pid the test did not spawn — would be
/// reported as reaped. `ESRCH` is the only answer that means *gone*, so it is the only one this
/// reads as one.
pub fn alive(pid: i32) -> bool {
    // SAFETY: `kill` with signal 0 delivers nothing; it only reports whether the pid is reachable.
    if unsafe { kill(pid, SIGNULL) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(ESRCH)
}

/// Kill a leaked process outright, ignoring the result.
///
/// The result is ignored on purpose and in one place, so no call site has to decide: by the time a
/// test is killing survivors it has already recorded the leak as its verdict, and a kill that fails
/// because the process finally exited on its own must not become a second, misleading failure.
pub fn kill_hard(pid: i32) {
    // SAFETY: `kill` is a plain libc call; a pid that no longer exists yields `ESRCH`, which is the
    // ignored case above.
    let _ = unsafe { kill(pid, SIGKILL) };
}

/// Processes still alive with `needle` on their command line, as `(pid, line)`.
///
/// **Every way `ps` can fail to answer is a failure of the test rather than an empty answer.**
/// `ps` is the only witness a leak check has, so reporting "no survivors" because it was missing,
/// errored, or printed nothing would make the assertion pass for free on exactly the machines where
/// it cannot be checked — the silent pass the check exists to rule out.
///
/// **The pid is parsed, and an unparsable one panics.** That is the half that failed to propagate:
/// two of the five copies this replaces returned the raw line instead, so a matching line whose pid
/// would not parse was silently dropped — a survivor the test could not name, discarded by the code
/// whose entire job was to name it. Dropping it is the same silent pass one line down.
pub fn survivors(needle: &str) -> Vec<(i32, String)> {
    let out = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .expect("`ps` must run: without it nothing here can tell a clean run from a leak");
    assert!(
        out.status.success(),
        "`ps -axo pid=,command=` exited {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let listing = String::from_utf8_lossy(&out.stdout);
    // `ps -ax` lists at minimum the calling test process, so an empty listing means a witness that
    // did not work, not a machine with nothing running on it.
    assert!(
        !listing.trim().is_empty(),
        "`ps` printed nothing; the leak check would report no survivors whatever had leaked"
    );
    listing
        .lines()
        .filter(|l| l.contains(needle))
        .map(|l| {
            let pid = l
                .split_whitespace()
                .next()
                .and_then(|p| p.parse().ok())
                .unwrap_or_else(|| {
                    panic!("`ps` line matches {needle:?} but carries no pid: {l:?}")
                });
            (pid, l.to_string())
        })
        .collect()
}

/// Is `program` on `PATH` and runnable?
///
/// Callers assert on this and name the binary rather than skipping: §9's standing rule is that a
/// criterion which quietly passes on a machine that cannot run it is worth less than no criterion.
pub fn on_path(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// --- scratch directories -------------------------------------------------------------------------

/// A scratch dir that removes itself.
///
/// `Drop`, and not a `remove_dir_all` at the end of each test: a failing assertion unwinds straight
/// past any trailing cleanup, so an explicit call leaks on exactly the runs that fail — the ones a
/// developer re-runs most. `Drop` catches those, plus every `?`, every early return and every
/// `.expect` in a helper the test called. See `scratch_removes_its_dir_when_a_test_panics`, which
/// is that property as a test rather than as a claim.
pub struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        // Ignored: the dir may already be gone (a test that removed it, or a `git worktree remove`
        // that took it), and a cleanup failure must not mask the test's own verdict.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

/// `Deref` alone is not enough: it coerces `&Scratch` to `&Path` at a call site expecting one, but
/// a generic `P: AsRef<Path>` — `std::fs::remove_dir_all`, `Command::current_dir` — never triggers
/// that coercion and fails to compile instead. Two of the eleven copies this replaces had learnt
/// that separately; the other nine made their call sites write `&*dir`.
impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// A fresh scratch dir under the system temp dir, named `marion-{tag}-{pid}-{thread}`.
///
/// **Bind the returned guard for as long as the test needs the directory.** `scratch("x").join("y")`
/// drops the guard at the end of that statement and deletes the dir out from under the run, and a
/// bare `let _ =` drops it on the spot. That is the one trap in this shape, and it is the reason
/// the constructor returns the guard rather than a path.
///
/// Three properties, each of which some copy of this had and some did not:
///
/// - **the thread id is in the name**, so two tests in one binary — which `cargo test` runs in
///   parallel by default — cannot collide on a tag;
/// - **the dir is removed on the way *in* as well**, because a run killed hard enough to skip
///   `Drop` leaves a name behind and pids recycle, so a later run can inherit that exact name;
/// - **the path is canonicalised**, because on macOS the temp dir is a symlink (`/var` →
///   `/private/var`) and a test that compares a path marion reported against one built here
///   otherwise compares two spellings of the same directory.
pub fn scratch(tag: &str) -> Scratch {
    let p = std::env::temp_dir().join(format!(
        "marion-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    Scratch(p.canonicalize().expect("scratch dir canonicalises"))
}

// --- git ------------------------------------------------------------------------------------------

/// `git` in `dir`, asserting it succeeded and handing back its stdout.
///
/// Returning the output rather than `()` is the superset: the five copies that discarded it keep
/// compiling unchanged, and the two that needed it stop being a separate function.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A one-commit repository for a child to worktree, at `root/repo`.
///
/// Its own repository and never marion's: the run under test writes to it. The identity is passed
/// per-invocation rather than configured globally, so the fixture does not depend on — or disturb —
/// whatever `user.email` the machine running the suite has.
pub fn fixture_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/keep.txt"), "keep\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main", "."]);
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=marion@example.invalid",
            "-c",
            "user.name=marion",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    repo
}

// --- persisted contracts ---------------------------------------------------------------------------

/// One `contracts/<task_id>.json` marion persisted, and what reading it back produced.
///
/// The parse outcome is carried rather than acted on — see [`persisted_contracts`] for why the
/// judgement is deliberately somewhere else.
pub struct PersistedContract {
    pub path: PathBuf,
    /// `Err` holds the diagnosis, already formatted: an unreadable file, or one whose bytes are not
    /// JSON, together with the head of what was actually there.
    pub parsed: Result<Value, String>,
}

/// Every `contracts/<task_id>.json` under `state`, read back — **without judging any of them**.
///
/// # Why the walk is fallible and the judgement is not here
///
/// These two look like competing designs and are not. The walk propagates its `read_dir` errors
/// rather than swallowing them, because a directory the test could not enumerate is a contract it
/// cannot claim is absent, and a helper that answers "none" to that question makes a count
/// assertion pass for the wrong reason.
///
/// But the *judgement* — panicking over a contract that will not read back — must not happen here,
/// and that is the part worth stating. A caller runs this while its canned provider is still up and
/// its scratch dir still on disk. Panicking at the read site unwinds through both, turning one
/// corrupt contract into the leaked processes and stranded directory the caller's next block exists
/// to prevent. So the shape is: **fallible walk, collected results, judgement at a point the caller
/// chooses — after the server is dropped and the survivor sweep has run.** [`judge`] is that point.
///
/// The count and the parse are still the same question, which is why the parse happens here at all:
/// a `filter_map` that silently dropped an unparsable file made one good contract beside one
/// corrupt one count as one, and pass.
pub fn persisted_contracts(state: &Path) -> std::io::Result<Vec<PersistedContract>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let p = entry?.path();
            if p.is_dir() {
                walk(&p, out)?;
            } else if p.extension().is_some_and(|x| x == "json")
                && p.parent().is_some_and(|d| d.ends_with("contracts"))
            {
                out.push(p);
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    walk(state, &mut paths)?;
    // Stable across runs and machines: `read_dir` order is not, and a caller indexing `[0]` after
    // asserting a count of one should get the same file every time it fails.
    paths.sort();
    Ok(paths
        .into_iter()
        .map(|path| {
            let parsed = std::fs::read(&path)
                .map_err(|e| {
                    format!(
                        "marion persisted {} and this test cannot read it back: {e}",
                        path.display()
                    )
                })
                .and_then(|bytes| {
                    serde_json::from_slice(&bytes).map_err(|e| {
                        format!(
                            "{} is not JSON: {e}\nfirst 512 bytes:\n{}",
                            path.display(),
                            String::from_utf8_lossy(&bytes)
                                .chars()
                                .take(512)
                                .collect::<String>()
                        )
                    })
                });
            PersistedContract { path, parsed }
        })
        .collect())
}

/// The judgement [`persisted_contracts`] deliberately withholds: a contract marion wrote and cannot
/// read back is a defect whichever half is wrong, so this panics naming the file.
///
/// **Call it after cleanup, not before.** That is the whole reason it is a second function.
pub fn judge(contracts: Vec<PersistedContract>) -> Vec<(PathBuf, Value)> {
    contracts
        .into_iter()
        .map(|c| match c.parsed {
            Ok(v) => (c.path, v),
            Err(diagnosis) => panic!("{diagnosis}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The property every `Scratch` in this repo was introduced for, and the one nobody had
    /// written down.** It has been proved by hand — forcing a failure, listing `$TMPDIR` — three
    /// separate times in one session, once per leaking call site. A property proved by hand three
    /// times should be a test.
    ///
    /// `catch_unwind` rather than a `#[should_panic]` test, because the assertion is about what is
    /// on disk *after* the unwind, which a `#[should_panic]` test has no way to check.
    #[test]
    fn scratch_removes_its_dir_when_a_test_panics() {
        let payload = std::panic::catch_unwind(|| {
            let dir = scratch("unwind-probe");
            assert!(dir.is_dir(), "the probe needs a dir to lose");
            std::fs::write(dir.join("work.txt"), b"in progress").unwrap();
            panic!("a test failing with work in progress");
        })
        .expect_err("the probe panics on purpose");
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .unwrap_or_default();
        assert!(
            message.contains("work in progress"),
            "the probe's own panic must be what came back, not a failure inside the guard: \
             {message}"
        );
        // The guard was dropped by the unwind, so nothing it made is left. Re-derived from the same
        // inputs rather than smuggled out of the closure, which `catch_unwind` will not let it be.
        let expected = std::env::temp_dir().join(format!(
            "marion-unwind-probe-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        assert!(
            !expected.exists(),
            "a panicking test left {} behind — the exact asymmetry `Drop` replaced a trailing \
             `remove_dir_all` to remove",
            expected.display()
        );
    }

    #[test]
    fn scratch_removes_its_dir_on_an_ordinary_return() {
        let path = {
            let dir = scratch("ordinary");
            assert!(dir.is_dir());
            dir.to_path_buf()
        };
        assert!(!path.exists(), "{} outlived its guard", path.display());
    }

    /// The entry-side removal, which the exit-side one does not make redundant: a run killed hard
    /// enough to skip `Drop` leaves the name behind, and pids recycle.
    #[test]
    fn scratch_starts_from_an_empty_dir_even_when_the_name_was_already_taken() {
        let first = scratch("reused");
        let path = first.to_path_buf();
        std::fs::write(path.join("stale.txt"), b"from a run that died").unwrap();
        // Deliberately leaked: this is the state a killed run leaves, which `Drop` never got to.
        std::mem::forget(first);
        assert!(
            path.join("stale.txt").exists(),
            "the probe needs the stale file"
        );

        let second = scratch("reused");
        assert_eq!(*second, *path, "the same tag claims the same name");
        assert!(
            !second.join("stale.txt").exists(),
            "a re-run inherited the previous run's contents, so anything it asserted about an \
             empty tree would have been asserting about someone else's leftovers"
        );
    }

    /// `Deref` covers `&Scratch` where `&Path` is expected; `AsRef` covers a generic `P: AsRef<Path>`,
    /// which never triggers deref coercion. Both are load-bearing at real call sites, so both are
    /// exercised here — this test failing to *compile* is the assertion.
    #[test]
    fn scratch_is_accepted_both_by_a_path_parameter_and_by_a_generic_asref() {
        fn takes_path(p: &Path) -> bool {
            p.is_dir()
        }
        fn takes_asref<P: AsRef<Path>>(p: P) -> bool {
            p.as_ref().is_dir()
        }
        let dir = scratch("coercions");
        assert!(takes_path(&dir));
        assert!(takes_asref(&dir));
    }

    /// The needle is this process's own command line, which is guaranteed present — so a `survivors`
    /// that returned nothing here would be broken rather than reporting a clean machine.
    #[test]
    fn survivors_finds_a_live_process_and_parses_its_pid() {
        let needle = format!(
            "marion-survivor-probe-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        // `ps` reports argv, so the needle has to *be* in argv. BSD `sleep` takes exactly one
        // operand and would exit on a second, so the probe is a shell whose own `-c` string carries
        // the needle in a comment.
        //
        // **The `; true` is load-bearing.** A `sh -c` whose script is a single simple command is
        // `exec`ed in place by every shell here, which replaces the shell's argv with `sleep 30`
        // and takes the needle with it — measured: this test failed exactly that way without it. A
        // second command makes the shell a real process for the duration.
        let mut child = Command::new("sh")
            .args(["-c", &format!("sleep 30; true # {needle}")])
            .spawn()
            .expect("the probe process starts");
        // `spawn` returns before the child is necessarily in `ps` output.
        std::thread::sleep(std::time::Duration::from_millis(200));

        let found = survivors(&needle);
        assert!(
            !found.is_empty(),
            "a process with {needle:?} in its command line is running and `survivors` did not see it"
        );
        assert!(
            found.iter().any(|(pid, _)| *pid == child.id() as i32),
            "the parsed pid must be the probe's own {}: {found:?}",
            child.id()
        );

        kill_hard(child.id() as i32);
        let _ = child.wait();
    }

    #[test]
    fn survivors_is_empty_for_a_needle_nothing_carries() {
        let needle = format!(
            "marion-nothing-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        assert!(survivors(&needle).is_empty());
    }

    /// **The half that a naive `kill(pid, 0) == 0` gets wrong, and that this crate nearly shipped
    /// wrong.** `pid 1` is `launchd`/`init`: it always exists and is owned by root, so `kill(1, 0)`
    /// from a test process returns `-1` with `EPERM`. A version that read any non-zero return as a
    /// death would call the most obviously-running process on the machine dead — and in a leak
    /// check, "dead" is the answer that passes.
    #[test]
    fn a_process_that_exists_but_is_not_ours_is_alive_rather_than_gone() {
        assert!(
            alive(1),
            "pid 1 exists and is not ours: `kill` answers EPERM, which is a survivor and not a \
             death. Only ESRCH means gone"
        );
    }

    #[test]
    fn alive_tracks_a_process_across_its_death() {
        let mut child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("probe starts");
        let pid = child.id() as i32;
        assert!(alive(pid), "a just-spawned `sleep 30` is alive");
        kill_hard(pid);
        // Reaped before asking again: a killed child stays a zombie — and therefore signalable —
        // until its parent waits on it, so `alive` would keep answering true.
        let _ = child.wait();
        assert!(!alive(pid), "the probe was killed and reaped");
    }

    #[test]
    fn on_path_answers_for_a_program_that_exists_and_one_that_cannot() {
        assert!(on_path("git"), "this suite cannot run without git");
        assert!(!on_path("marion-no-such-program-anywhere"));
    }

    #[test]
    fn a_fixture_repo_is_a_real_one_commit_repository() {
        let dir = scratch("fixture-repo");
        let repo = fixture_repo(&dir);
        assert_eq!(repo, dir.join("repo"));
        assert_eq!(
            std::fs::read_to_string(repo.join("src/keep.txt")).unwrap(),
            "keep\n"
        );
        assert_eq!(
            git(&repo, &["rev-list", "--count", "HEAD"]).trim(),
            "1",
            "one commit, so a caller's base_commit is unambiguous"
        );
        assert!(
            git(&repo, &["status", "--porcelain"]).trim().is_empty(),
            "a fixture that starts dirty would put its own noise in every changed_paths"
        );
    }

    #[test]
    fn persisted_contracts_reads_back_what_is_under_a_contracts_dir() {
        let dir = scratch("contracts-ok");
        let contracts = dir.join("proj/agents/a1/contracts");
        std::fs::create_dir_all(&contracts).unwrap();
        std::fs::write(contracts.join("t1.json"), br#"{"task_id":"t1"}"#).unwrap();
        // Neither of these is a contract: one is the wrong extension, the other the wrong dir.
        std::fs::write(contracts.join("notes.txt"), b"ignore me").unwrap();
        std::fs::write(dir.join("proj/agents/a1/loose.json"), b"{}").unwrap();

        let found = judge(persisted_contracts(&dir).expect("the walk succeeds"));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, contracts.join("t1.json"));
        assert_eq!(found[0].1["task_id"], "t1");
    }

    /// The half that must NOT panic where it is read. A corrupt contract is carried out as data, so
    /// the caller can drop its provider and sweep for survivors before judging it.
    #[test]
    fn a_corrupt_contract_is_carried_out_rather_than_panicking_at_the_read() {
        let dir = scratch("contracts-corrupt");
        let contracts = dir.join("proj/agents/a1/contracts");
        std::fs::create_dir_all(&contracts).unwrap();
        std::fs::write(contracts.join("bad.json"), b"{ this is not json").unwrap();

        let found = persisted_contracts(&dir).expect("the walk still succeeds");
        assert_eq!(found.len(), 1, "a file that will not parse is still a file");
        let diagnosis = found[0]
            .parsed
            .as_ref()
            .expect_err("it does not parse")
            .clone();
        assert!(diagnosis.contains("is not JSON"), "{diagnosis}");
        assert!(
            diagnosis.contains("this is not json"),
            "the diagnosis carries what was actually there: {diagnosis}"
        );

        // And judging it is what panics — at a point the caller chose.
        let panicked = std::panic::catch_unwind(|| judge(persisted_contracts(&dir).unwrap()));
        assert!(panicked.is_err(), "judge must not tolerate it either");
    }

    /// A state dir that cannot be enumerated is not a state dir with no contracts in it.
    #[test]
    fn a_walk_that_cannot_read_a_directory_is_an_error_and_not_an_empty_answer() {
        let dir = scratch("contracts-missing");
        let gone = dir.join("never-created");
        assert!(
            persisted_contracts(&gone).is_err(),
            "answering `no contracts` for a directory that could not be read makes a count \
             assertion pass for the wrong reason"
        );
    }
}
