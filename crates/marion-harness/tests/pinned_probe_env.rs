//! The version gate's probe env is the rows' no-self-update env, pair for pair.
//!
//! `marion_testsupport::PINNED_HARNESSES` cannot depend on this crate, so it spells each
//! harness's `probe_env` itself. This is the check that the spelling is the row's: a gate that
//! probes without the env a node runs with reads the version of whatever build the harness would
//! download and exec for itself (copilot 1.0.83 answered `1.0.87` on 2026-09-22), and an admission
//! taken on that reading credits the wrong release.

use marion_core::Harness;
use marion_harness::adapter::harness_spec;
use marion_testsupport::PINNED_HARNESSES;

#[test]
fn every_pinned_harness_probes_with_its_rows_update_env() {
    for pin in PINNED_HARNESSES {
        let row = Harness::ALL
            .into_iter()
            .map(harness_spec)
            .find(|spec| spec.program == Some(pin.program))
            .unwrap_or_else(|| panic!("pinned harness {:?} has no row", pin.program));
        let from_row: Vec<(String, String)> = row.updates.env().into_iter().collect();
        let probed: Vec<(String, String)> = pin
            .probe_env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(
            probed, from_row,
            "{}: the gate's probe_env must equal the row's UpdatePolicy env",
            pin.program
        );
    }
}
