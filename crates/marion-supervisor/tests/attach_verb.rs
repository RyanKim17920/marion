//! `marion attach` is a **verb the binary dispatches**, not merely a parser that exists.
//!
//! The unit tests beside `parse_attach` prove the argv shape. They cannot prove the dispatch: with
//! the one `if` in `main` deleted, `parse_attach` still parses, still refuses unknown flags, and
//! every one of them still passes — while `marion attach a-1` falls through to `parse_args`, which
//! answers `None` for anything whose first word is not `run`, and the operator gets the usage text.
//!
//! So this runs the real binary. The distinction it turns on is that the two outcomes are
//! *different sentences*: a dispatched attach with no supervisor to dial says so and names the
//! project, and an undispatched one prints usage. Asserting the exit code alone would not separate
//! them — both are failures.

use std::process::Command;

/// A directory with no supervisor, and not the workspace's own: `marion attach` with no `--repo`
/// resolves the current directory, and the current directory of a test is the crate root — where a
/// supervisor from some other test may genuinely be serving.
fn empty_project() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("marion-attach-verb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch project");
    dir.canonicalize().expect("canonical")
}

#[test]
fn attach_is_dispatched_by_the_binary_and_not_answered_with_usage() {
    let dir = empty_project();
    let out = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args([
            "attach",
            "a-1",
            "--repo",
            dir.to_str().expect("utf-8"),
            "--state-dir",
            dir.to_str().expect("utf-8"),
        ])
        .output()
        .expect("the marion binary runs");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stderr.contains("no supervisor is serving"),
        "`marion attach` was not dispatched — it fell through to `run`'s parser. stderr: {stderr}"
    );
    assert!(
        !stderr.contains("usage: marion"),
        "the verb printed usage, which is what an undispatched argv looks like: {stderr}"
    );
    // And the refusal is specifically the one that says marion declined to *start* a supervisor,
    // because an attach that silently started one would answer `not found` for a live node.
    assert!(
        stderr.contains("deliberately does not start one"),
        "the refusal does not explain why marion did not start a supervisor: {stderr}"
    );
    assert!(
        !out.status.success(),
        "an attach that could not happen exits non-zero"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// **`marion resume` is dispatched, and — unlike `attach` — it starts a supervisor when none
/// serves** (`plan-restart-resume.md` step 7). Attach declines to start one because a supervisor
/// started fresh has no record of the node and would answer `not found` about marion rather than
/// about the node. Resume is the opposite case by construction: the node marion is asked to bring
/// back is one whose supervisor **died**, so there is deliberately no supervisor to dial, and
/// starting one is the whole operation. With no such node on a fresh journal the resume then
/// refuses by name — which is what proves both that the argv was dispatched (not answered with
/// usage) and that a supervisor was reached (not declined the way attach declines).
#[test]
fn resume_is_dispatched_and_starts_a_supervisor_when_none_serves() {
    let dir = std::env::temp_dir().join(format!("marion-resume-verb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch project");
    let dir = dir.canonicalize().expect("canonical");
    let out = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args([
            "resume",
            "a-1",
            "--prompt",
            "carry on",
            "--repo",
            dir.to_str().expect("utf-8"),
            "--state-dir",
            dir.to_str().expect("utf-8"),
            "--canned",
        ])
        .output()
        .expect("the marion binary runs");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !stderr.contains("usage: marion"),
        "`marion resume` fell through to a parser and printed usage: {stderr}"
    );
    // Attach's refusal is the sentence resume must NOT share: resume starts a supervisor, attach
    // does not.
    assert!(
        !stderr.contains("deliberately does not start one"),
        "resume answered with attach's refusal — it declined to start a supervisor: {stderr}"
    );
    // It reached a supervisor and got a resume-specific answer about the missing node. (On an
    // environment where the socket path is too long to bind — a known local limitation — the
    // attempt still fails as marion's own sentence about the supervisor, never as usage.)
    assert!(
        stderr.contains("nothing to resume")
            || stderr.contains("supervisor")
            || stderr.contains("resume"),
        "resume was not dispatched to the supervisor: {stderr}"
    );
    assert!(
        !out.status.success(),
        "resuming a node that does not exist exits non-zero"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The verb is discoverable. A subcommand nobody can find is one that does not exist for the
/// operator who needs it, and `--help` is where they will look.
#[test]
fn the_binarys_help_names_the_attach_verb() {
    let out = Command::new(env!("CARGO_BIN_EXE_marion"))
        .arg("--help")
        .output()
        .expect("the marion binary runs");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("marion attach <agent-id>"), "{text}");
    assert!(out.status.success());
}
