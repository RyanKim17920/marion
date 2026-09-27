//! **A harness newer than every admitted release is never refused at runtime.**
//!
//! Harnesses update themselves (claude, codex and opencode all moved overnight on 2026-09-06), so
//! the operator's `claude` is routinely a release marion's tests have not seen. The version gate
//! that refuses such a release, `marion_testsupport::PINNED_HARNESSES`, exists for the *tests*: it
//! keeps "the matrix passed" meaning "the matrix passed on the release it names". It must never
//! become a reason the shipped `marion` declines to launch a node. Two things would make it one,
//! and each has a check here:
//!
//! 1. the gate crate linked into a shipped binary — it is a dev-dependency everywhere, and
//!    [`the_version_gate_is_linked_into_no_shipped_crate`] keeps it one;
//! 2. the one production table keyed on version, [`marion_harness::advertised`], withdrawing a
//!    capability from a release *newer* than an admitted one. Its version floors are lower bounds
//!    on what was measured, so a newer release keeps everything the admitted one had;
//!    [`a_newer_release_keeps_every_capability_of_the_newest_admitted_one`] holds it to that.

use std::path::Path;

use marion_core::Harness;
use marion_harness::adapter::harness_spec;
use marion_harness::advertised;
use marion_testsupport::PINNED_HARNESSES;

/// Releases after `version`: the next patch, minor and major, and one far past all of them.
fn newer_than(version: &str) -> Vec<String> {
    let parts: Vec<u64> = version
        .split('.')
        .map(|p| p.parse().expect("an admitted version is dotted numbers"))
        .collect();
    let [major, minor, patch] = parts[..] else {
        panic!("an admitted version is major.minor.patch: {version:?}");
    };
    vec![
        format!("{major}.{minor}.{}", patch + 1),
        format!("{major}.{}.0", minor + 1),
        format!("{}.0.0", major + 1),
        "999.0.0".to_string(),
    ]
}

#[test]
fn a_newer_release_keeps_every_capability_of_the_newest_admitted_one() {
    for pin in PINNED_HARNESSES {
        let harness = Harness::ALL
            .into_iter()
            .find(|h| harness_spec(*h).program == Some(pin.program))
            .unwrap_or_else(|| panic!("pinned harness {:?} has no row", pin.program));
        let newest = pin.accepted[pin.accepted.len() - 1];
        let admitted = advertised(harness, newest);
        for later in newer_than(newest) {
            let got = advertised(harness, &later);
            assert!(
                admitted.is_at_or_below(&got),
                "{} {later} advertises {:?}, less than admitted {newest}'s {:?}: a release newer \
                 than the pin table must never lose a capability for being unadmitted",
                pin.program,
                got.granted(),
                admitted.granted(),
            );
        }
    }
}

/// Every `marion-testsupport` mention in a workspace manifest sits under a dev-dependencies table.
///
/// Read as text rather than through a TOML parser so this crate gains no dependency for it: a
/// manifest line naming the crate is attributed to the most recent `[table]` header above it,
/// which is how Cargo reads it too.
#[test]
fn the_version_gate_is_linked_into_no_shipped_crate() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("this crate lives under crates/");
    let mut manifests = 0;
    for entry in std::fs::read_dir(crates).expect("crates/ is readable") {
        let manifest = entry
            .expect("a readable crates/ entry")
            .path()
            .join("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        manifests += 1;
        let mut table = String::new();
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                table = line.to_string();
            } else if line.starts_with("marion-testsupport") {
                assert!(
                    table.ends_with("dev-dependencies]"),
                    "{} depends on marion-testsupport under {table}: the version gate would reach \
                     a shipped binary and could refuse an operator's newer harness",
                    manifest.display()
                );
            }
        }
    }
    assert!(
        manifests >= 7,
        "found only {manifests} manifests under {}; the scan is not looking where the workspace is",
        crates.display()
    );
}
