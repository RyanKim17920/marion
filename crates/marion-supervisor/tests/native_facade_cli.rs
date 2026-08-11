//! The native facade seam is wired into the user-facing binary, but this slice advertises none.
//! These selectors therefore retain the legacy usage refusal until a later transport slice makes
//! one production descriptor ready.

use std::cell::Cell;
use std::ffi::OsString;
use std::process::{Command, ExitCode};

use marion_core::{
    Lane, NativeAdapterId, NativeFacadeDescriptor, NativeFacadeRegistry, NativeLane,
    VendorIdentity, production_native_facades,
};
use marion_supervisor::facade_cli::dispatch_native_facade_or_legacy;

#[test]
fn a_synthetic_ready_facade_is_refused_before_the_legacy_cli_runs() {
    const DESCRIPTORS: &[NativeFacadeDescriptor] = &[NativeFacadeDescriptor {
        identity: VendorIdentity::new("atlas"),
        command: "atlas",
        aliases: &[],
        native: Some(Lane::new(
            true,
            NativeLane::new("atlas-cli", "codex", NativeAdapterId::new("atlas-native")),
        )),
        structured: None,
    }];
    let registry = NativeFacadeRegistry::new(DESCRIPTORS).expect("synthetic registry is valid");
    let legacy_called = Cell::new(false);
    let mut stderr = Vec::new();

    let status = dispatch_native_facade_or_legacy(
        [OsString::from("atlas"), OsString::from("--help")],
        &registry,
        &mut stderr,
        || {
            legacy_called.set(true);
            ExitCode::SUCCESS
        },
    );

    assert_eq!(status, ExitCode::FAILURE);
    assert_eq!(
        stderr,
        b"marion: native facade transport is not ready for \"atlas\"\n"
    );
    assert!(
        !legacy_called.get(),
        "matched facade reached the legacy CLI"
    );
}

#[test]
fn registered_disabled_or_native_absent_facades_never_fall_through_to_legacy() {
    const DISABLED: NativeFacadeDescriptor = NativeFacadeDescriptor {
        identity: VendorIdentity::new("disabled"),
        command: "disabled",
        aliases: &[],
        native: Some(Lane::new(
            false,
            NativeLane::new(
                "disabled-cli",
                "codex",
                NativeAdapterId::new("disabled-native"),
            ),
        )),
        structured: None,
    };
    const ABSENT: NativeFacadeDescriptor = NativeFacadeDescriptor {
        identity: VendorIdentity::new("absent"),
        command: "absent",
        aliases: &[],
        native: None,
        structured: None,
    };
    let descriptors = [DISABLED, ABSENT];
    let registry = NativeFacadeRegistry::new(&descriptors).unwrap();

    for (selector, expected) in [
        (
            "disabled",
            b"marion: native lane is disabled for \"disabled\"\n".as_slice(),
        ),
        (
            "absent",
            b"marion: registered facade \"absent\" has no native lane\n".as_slice(),
        ),
    ] {
        let legacy_called = Cell::new(false);
        let mut stderr = Vec::new();

        let status = dispatch_native_facade_or_legacy(
            [OsString::from(selector)],
            &registry,
            &mut stderr,
            || {
                legacy_called.set(true);
                ExitCode::SUCCESS
            },
        );

        assert_eq!(status, ExitCode::FAILURE);
        assert_eq!(stderr, expected);
        assert!(!legacy_called.get(), "{selector:?} fell through to legacy");
    }
}

#[test]
fn production_exposes_no_facade_and_known_or_arbitrary_names_keep_legacy_usage() {
    assert!(
        production_native_facades()
            .enabled_native_commands()
            .is_empty(),
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
