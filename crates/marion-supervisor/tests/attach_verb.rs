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
