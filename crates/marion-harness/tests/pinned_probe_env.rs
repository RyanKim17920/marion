//! The version gate's probe switch is the rows' no-self-update switch, in every shape.
//!
//! `marion_testsupport::PINNED_HARNESSES` cannot depend on this crate, so it spells each
//! harness's `probe_env`, `probe_args` and `probe_document` itself. This is the check that the spelling is the row's: a gate that
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

/// The gate's argument pair and settings document are what `marion_harness::probe` would carry
/// for the same row — so a codex or gemini gate probe is never the bare run a 2026-09-27 probe of
/// gemini was, which updated the operator's install.
#[test]
fn every_pinned_harness_probes_with_its_rows_whole_switch() {
    use marion_harness::probe::{DocumentChannel, ProbeSwitch};
    for pin in PINNED_HARNESSES {
        let row = Harness::ALL
            .into_iter()
            .map(harness_spec)
            .find(|spec| spec.program == Some(pin.program))
            .unwrap_or_else(|| panic!("pinned harness {:?} has no row", pin.program));
        let switch = ProbeSwitch::for_row(row)
            .unwrap_or_else(|e| panic!("{}: a pinned harness the probe refuses: {e}", pin.program));
        let args: Vec<String> = pin.probe_args.iter().map(|a| a.to_string()).collect();
        let document = pin.probe_document.map(|d| {
            let body: serde_json::Value = serde_json::from_str(d.body).expect("JSON");
            (d.env, body)
        });
        match &switch {
            ProbeSwitch::Args(want) => assert_eq!(&args, want, "{}", pin.program),
            ProbeSwitch::Document {
                via: DocumentChannel::Env(key),
                body,
            } => {
                let want: serde_json::Value = serde_json::from_str(body).unwrap();
                assert_eq!(document, Some((*key, want)), "{}", pin.program);
            }
            ProbeSwitch::Document { via, .. } => {
                panic!(
                    "{}: the gate names documents by env only, not {via:?}",
                    pin.program
                )
            }
            ProbeSwitch::Env { .. } | ProbeSwitch::Unneeded => {}
        }
        if !matches!(switch, ProbeSwitch::Args(_)) {
            assert!(
                args.is_empty(),
                "{}: probe_args for a row with no pair",
                pin.program
            );
        }
        if !matches!(switch, ProbeSwitch::Document { .. }) {
            assert!(
                document.is_none(),
                "{}: a document for a row with none",
                pin.program
            );
        }
    }
}
