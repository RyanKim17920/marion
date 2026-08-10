//! The native facade seam is wired into the user-facing binary, but this slice advertises none.
//! These selectors therefore retain the legacy usage refusal until a later transport slice makes
//! one production descriptor ready.

use std::process::Command;

use marion_core::production_native_facades;

#[test]
fn production_exposes_no_facade_and_known_or_arbitrary_names_keep_legacy_usage() {
    assert!(
        production_native_facades().ready_commands().is_empty(),
        "a native facade became public before its transport exists"
    );

    let canonical_help = Command::new(env!("CARGO_BIN_EXE_marion"))
        .arg("--help")
        .output()
        .expect("the marion binary runs");
    assert!(
        canonical_help.status.success(),
        "the canonical marion help command no longer succeeds"
    );
    assert!(
        canonical_help.stderr.is_empty(),
        "the canonical marion help command printed to stderr"
    );

    for selector in ["claude", "codex", "definitely-not-a-facade"] {
        let output = Command::new(env!("CARGO_BIN_EXE_marion"))
            .arg(selector)
            .output()
            .expect("the marion binary runs");
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(2),
            "{selector:?} no longer takes the legacy refusal path; stderr: {stderr}"
        );
        assert_eq!(
            output.stderr, canonical_help.stdout,
            "{selector:?} no longer prints byte-exact legacy usage; stderr: {stderr}"
        );
        assert!(
            output.stdout.is_empty(),
            "{selector:?} unexpectedly produced facade output: {:?}",
            String::from_utf8_lossy(&output.stdout)
        );

        let help = Command::new(env!("CARGO_BIN_EXE_marion"))
            .args([selector, "--help"])
            .output()
            .expect("the marion binary runs");
        assert!(
            help.status.success(),
            "{selector:?} no longer retains the legacy all-argument help scan"
        );
        assert_eq!(
            help.stdout, canonical_help.stdout,
            "{selector:?} no longer prints byte-exact legacy help"
        );
        assert!(
            help.stderr.is_empty(),
            "{selector:?} printed help to stderr"
        );
    }
}
