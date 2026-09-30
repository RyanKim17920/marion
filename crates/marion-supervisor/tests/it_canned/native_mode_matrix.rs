use std::ffi::OsString;

use marion_core::{
    Lane, LaneReadiness, NativeAdapterId, NativeFacadeDescriptor, NativeFacadeLaunchMode,
    NativeFacadeRegistry, NativeLane, StructuredAdapterId, StructuredAgentIdentity,
    StructuredControl, StructuredLane, VendorIdentity,
};
use marion_supervisor::facade_cli::match_native_facade_request;
use marion_supervisor::native_intent::{
    LaunchIntent, StructuredLaunchOrigin, select_native_facade,
};

fn descriptor(native_enabled: bool, structured_enabled: bool) -> NativeFacadeDescriptor {
    NativeFacadeDescriptor {
        identity: VendorIdentity::new("codex"),
        command: "codex-native",
        aliases: &["cn"],
        native: Some(Lane::new(
            native_enabled,
            NativeLane::new("codex", "codex", NativeAdapterId::new("codex-native")),
        )),
        structured: Some(Lane::new(
            structured_enabled,
            StructuredLane::new(
                StructuredAgentIdentity::new("codex-acp", 1),
                StructuredControl::Acp,
                StructuredAdapterId::new("codex-structured"),
            ),
        )),
    }
}

#[test]
fn each_enablement_combination_exposes_only_the_requested_public_surface() {
    for (native_enabled, structured_enabled) in
        [(false, false), (false, true), (true, false), (true, true)]
    {
        let descriptors = [descriptor(native_enabled, structured_enabled)];
        let registry = NativeFacadeRegistry::new(&descriptors).unwrap();

        let native_match = match_native_facade_request([OsString::from("codex-native")], &registry);
        let native_match = native_match.expect("registered selector is recognized before auth");
        assert_eq!(native_match.requested_selector(), "codex-native");
        assert!(native_match.opaque_tail().is_empty());

        let structured = select_native_facade(
            &registry,
            LaunchIntent::structured("codex-native", StructuredLaunchOrigin::Cli),
        );
        assert_eq!(
            structured.as_ref().map(|selection| selection.mode()),
            structured_enabled.then_some(NativeFacadeLaunchMode::Structured),
            "structured selection for ({native_enabled}, {structured_enabled})"
        );
    }
}

#[test]
fn every_non_native_provenance_refuses_a_native_only_descriptor() {
    let mut descriptor = descriptor(true, false);
    descriptor.structured = None;
    let descriptors = [descriptor];
    let registry = NativeFacadeRegistry::new(&descriptors).unwrap();

    for origin in [
        StructuredLaunchOrigin::Cli,
        StructuredLaunchOrigin::RemoteMcp,
        StructuredLaunchOrigin::RemoteAcp,
        StructuredLaunchOrigin::Subagent,
    ] {
        assert!(
            select_native_facade(&registry, LaunchIntent::structured("codex-native", origin))
                .is_none(),
            "{origin:?} must not fall through to native"
        );
    }
}

#[test]
fn computed_readiness_changes_bindability_without_mutating_static_lane_policy() {
    let descriptors = [descriptor(true, true)];
    let registry = NativeFacadeRegistry::new(&descriptors).unwrap();
    let resolved = registry.resolve("codex-native").unwrap();
    let native = resolved.native_lane().unwrap();
    let structured = resolved.structured_lane().unwrap();
    let ready = LaneReadiness::evaluate(true, true, true, true, true);
    let blocked = LaneReadiness::evaluate(true, true, true, true, false);

    assert!(native.is_bindable(ready));
    assert!(!native.is_bindable(blocked));
    assert!(structured.is_bindable(ready));
    assert!(!structured.is_bindable(blocked));
    assert!(native.lane().enabled());
    assert!(structured.enabled());
}

#[test]
fn structured_lane_names_a_concrete_agent_protocol_and_adapter() {
    let descriptor = descriptor(false, true);
    let lane = descriptor.structured.unwrap();

    assert_eq!(lane.agent_identity().name(), "codex-acp");
    assert_eq!(lane.agent_identity().protocol_version(), 1);
    assert_eq!(lane.control(), StructuredControl::Acp);
    assert_eq!(lane.adapter().as_str(), "codex-structured");
}
