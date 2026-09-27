//! **What the two binaries say to someone who has just installed them**, before any harness runs:
//! the version they are, and what an unrecognised command gets back.
//!
//! Every case here runs the built binary with a throwaway state directory and starts no harness,
//! so the file is quick and needs nothing on `PATH`.

use std::process::{Command, Output};

fn run(program: &str, args: &[&str]) -> Output {
    let state = std::env::temp_dir().join(format!("marion-cli-surface-{}", std::process::id()));
    Command::new(program)
        .args(args)
        .env("MARION_STATE_DIR", &state)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("could not run {program}: {e}"))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// **`--version` names the binary and the workspace version, and succeeds.** Both used to print
/// their usage text and exit 2, so nobody could tell which marion they had.
#[test]
fn both_binaries_print_their_version_and_exit_zero() {
    let version = env!("CARGO_PKG_VERSION");
    for (program, name) in [
        (env!("CARGO_BIN_EXE_marion"), "marion"),
        (env!("CARGO_BIN_EXE_marion-supervisor"), "marion-supervisor"),
    ] {
        for flag in ["--version", "-V"] {
            let out = run(program, &[flag]);
            assert!(out.status.success(), "{name} {flag}: {:?}", out.status);
            assert_eq!(
                text(&out.stdout).trim_end(),
                format!("{name} {version}"),
                "{name} {flag}"
            );
        }
    }
}
