//! Native authority has no public constructor or public CLI minting proxy.
//!
//! ```compile_fail
//! use std::ffi::OsString;
//! use marion_core::production_native_facades;
//! use marion_supervisor::facade_cli::resolve_native_invocation;
//!
//! let registry = production_native_facades();
//! let _forged = resolve_native_invocation([OsString::from("codex")], &registry);
//! ```
//!
//! Even naming the sealed types does not let an external caller construct native provenance.
//!
//! ```compile_fail
//! use marion_supervisor::native_intent::{AuthorizedNativeFacade, LaunchIntent};
//!
//! let authorization = AuthorizedNativeFacade::new("codex");
//! let _forged = LaunchIntent::native(authorization);
//! ```
//!
//! Selection consumes its intent; even structured callers cannot reuse provenance accidentally.
//!
//! ```compile_fail
//! use marion_core::production_native_facades;
//! use marion_supervisor::native_intent::{
//!     LaunchIntent, StructuredLaunchOrigin, select_native_facade,
//! };
//!
//! let registry = production_native_facades();
//! let intent = LaunchIntent::structured("codex", StructuredLaunchOrigin::Cli);
//! let _first = select_native_facade(&registry, intent);
//! let _reused = select_native_facade(&registry, intent);
//! ```

use crate::native_bootstrap::{ConsumedNativeCapability, ConsumedNativeRequest};
use crate::native_tty::ControllingTtyWitness;
#[cfg(test)]
use marion_core::LaneReadiness;
use marion_core::{
    NativeFacadeDescriptor, NativeFacadeLaunchMode, NativeFacadeRegistry,
    NativeFacadeStructuredLane, ResolvedNativeFacade, ResolvedNativeFacadeNativeLane,
};

/// A direct-CLI native request whose provenance was established inside the supervisor crate.
///
/// The value remains move-only so selection consumes the authorization decision exactly once.
#[derive(Debug)]
pub struct AuthorizedNativeFacade<'request> {
    selector: &'request str,
}

impl<'request> AuthorizedNativeFacade<'request> {
    pub(crate) const fn from_consumed(proof: ConsumedNativeCapability<'request>) -> Self {
        Self {
            selector: proof.selector(),
        }
    }
}

/// Provenance for requests that must remain on the structured lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuredLaunchOrigin {
    Cli,
    RemoteMcp,
    RemoteAcp,
    Subagent,
}

/// A closed launch request. External callers can construct only structured provenance.
#[derive(Debug)]
pub struct LaunchIntent<'request> {
    kind: LaunchIntentKind<'request>,
}

#[derive(Debug)]
enum LaunchIntentKind<'request> {
    #[allow(
        dead_code,
        reason = "production stays dark until Task 2 can issue authorization"
    )]
    Native(AuthorizedNativeFacade<'request>),
    Structured {
        selector: &'request str,
        origin: StructuredLaunchOrigin,
    },
}

impl<'request> LaunchIntent<'request> {
    #[allow(
        dead_code,
        reason = "production stays dark until Task 2 can issue authorization"
    )]
    const fn native(authorized: AuthorizedNativeFacade<'request>) -> Self {
        Self {
            kind: LaunchIntentKind::Native(authorized),
        }
    }

    /// Constructs an explicitly structured request; it cannot select or fall back to native.
    pub const fn structured(selector: &'request str, origin: StructuredLaunchOrigin) -> Self {
        Self {
            kind: LaunchIntentKind::Structured { selector, origin },
        }
    }

    const fn selector(&self) -> &'request str {
        match &self.kind {
            LaunchIntentKind::Native(authorized) => authorized.selector,
            LaunchIntentKind::Structured { selector, .. } => selector,
        }
    }
}

/// A native lane selected only after sealed direct-CLI authorization was consumed.
#[derive(Debug, PartialEq, Eq)]
pub struct SelectedNativeFacade<'registry> {
    resolved: ResolvedNativeFacade<'registry>,
}

impl<'registry> SelectedNativeFacade<'registry> {
    pub const fn descriptor(&self) -> &'registry NativeFacadeDescriptor {
        self.resolved.descriptor()
    }

    pub fn native_lane(&self) -> ResolvedNativeFacadeNativeLane<'registry> {
        self.resolved
            .native_lane()
            .expect("selected native facade contains a native lane")
    }
}

/// A selected native facade paired with current computed readiness evidence.
///
/// This type has no production issuer until the readiness owner lands. The binder consumes it and
/// therefore cannot accept caller-asserted readiness facts directly.
#[derive(Debug, PartialEq, Eq)]
pub struct ReadyNativeFacade<'registry> {
    selected: SelectedNativeFacade<'registry>,
}

impl<'registry> ReadyNativeFacade<'registry> {
    pub const fn descriptor(&self) -> &'registry NativeFacadeDescriptor {
        self.selected.descriptor()
    }

    pub fn native_lane(&self) -> ResolvedNativeFacadeNativeLane<'registry> {
        self.selected.native_lane()
    }
}

/// A structured lane selected from explicit non-native provenance.
#[derive(Debug, PartialEq, Eq)]
pub struct SelectedStructuredFacade<'registry> {
    resolved: ResolvedNativeFacade<'registry>,
    origin: StructuredLaunchOrigin,
}

impl<'registry> SelectedStructuredFacade<'registry> {
    pub const fn descriptor(&self) -> &'registry NativeFacadeDescriptor {
        self.resolved.descriptor()
    }

    pub const fn origin(&self) -> StructuredLaunchOrigin {
        self.origin
    }

    pub const fn lane(&self) -> &'registry NativeFacadeStructuredLane {
        self.resolved
            .structured_lane()
            .expect("selected structured facade contains a structured lane")
    }
}

/// One descriptor and exactly one statically enabled lane selected from a closed intent.
#[derive(Debug, PartialEq, Eq)]
pub struct NativeFacadeSelection<'registry> {
    kind: NativeFacadeSelectionKind<'registry>,
}

#[derive(Debug, PartialEq, Eq)]
enum NativeFacadeSelectionKind<'registry> {
    Native(SelectedNativeFacade<'registry>),
    Structured(SelectedStructuredFacade<'registry>),
}

impl<'registry> NativeFacadeSelection<'registry> {
    pub const fn mode(&self) -> NativeFacadeLaunchMode {
        match self.kind {
            NativeFacadeSelectionKind::Native(_) => NativeFacadeLaunchMode::Native,
            NativeFacadeSelectionKind::Structured(_) => NativeFacadeLaunchMode::Structured,
        }
    }

    pub const fn structured(&self) -> Option<&SelectedStructuredFacade<'_>> {
        match &self.kind {
            NativeFacadeSelectionKind::Native(_) => None,
            NativeFacadeSelectionKind::Structured(selection) => Some(selection),
        }
    }

    #[allow(dead_code, reason = "Task 2 is the first production native consumer")]
    fn into_native(self) -> Option<SelectedNativeFacade<'registry>> {
        match self.kind {
            NativeFacadeSelectionKind::Native(selection) => Some(selection),
            NativeFacadeSelectionKind::Structured(_) => None,
        }
    }
}

/// Consumes bootstrap authorization into native-only selection without exposing a constructor for
/// that authorization to external callers.
#[allow(
    dead_code,
    reason = "Task 3 receives authorization through ConsumedNativeRequest::into_parts"
)]
pub(crate) fn select_consumed_native<'registry>(
    registry: &'registry NativeFacadeRegistry<'_>,
    authorization: AuthorizedNativeFacade<'_>,
) -> Option<SelectedNativeFacade<'registry>> {
    select_native_facade(registry, LaunchIntent::native(authorization))?.into_native()
}

/// Consume Task 2's authenticated request before resolving any descriptor or lane detail.
pub(crate) fn select_consumed_direct_cli<'registry>(
    registry: &'registry NativeFacadeRegistry<'_>,
    consumed: ConsumedNativeRequest<'_>,
) -> Option<(SelectedNativeFacade<'registry>, ControllingTtyWitness)> {
    let (_context, _hash, terminal, authorization) = consumed.into_parts().into_components();
    let selected = select_consumed_native(registry, authorization)?;
    let terminal = terminal.into_controlling_tty_witness()?;
    #[cfg(test)]
    crate::native_bootstrap::observe_native_route_test_stage(
        crate::native_bootstrap::NativeRouteTestStage::SelectorSelected,
    );
    Some((selected, terminal))
}

#[cfg(test)]
pub(crate) fn select_test_native<'registry>(
    registry: &'registry NativeFacadeRegistry<'_>,
    selector: &str,
) -> Option<SelectedNativeFacade<'registry>> {
    select_native_facade(
        registry,
        LaunchIntent::native(AuthorizedNativeFacade { selector }),
    )?
    .into_native()
}

#[cfg(test)]
pub(crate) fn ready_test_native(
    selected: SelectedNativeFacade<'_>,
    readiness: LaneReadiness,
) -> Option<ReadyNativeFacade<'_>> {
    selected
        .native_lane()
        .is_bindable(readiness)
        .then_some(ReadyNativeFacade { selected })
}

/// Resolves only the statically enabled lane named by trusted request provenance.
///
/// Current readiness remains a later bind-time fact and is never read from the descriptor.
pub fn select_native_facade<'registry>(
    registry: &'registry NativeFacadeRegistry<'_>,
    intent: LaunchIntent<'_>,
) -> Option<NativeFacadeSelection<'registry>> {
    let resolved = registry.resolve(intent.selector())?;
    let kind = match intent.kind {
        LaunchIntentKind::Native(_) if resolved.native_lane()?.lane().enabled() => {
            NativeFacadeSelectionKind::Native(SelectedNativeFacade { resolved })
        }
        LaunchIntentKind::Structured { origin, .. } if resolved.structured_lane()?.enabled() => {
            NativeFacadeSelectionKind::Structured(SelectedStructuredFacade { resolved, origin })
        }
        _ => return None,
    };

    Some(NativeFacadeSelection { kind })
}

#[cfg(test)]
mod tests {
    use marion_core::{
        Lane, NativeAdapterId, NativeFacadeNativeLane, NativeLane, StructuredAdapterId,
        StructuredAgentIdentity, StructuredControl, StructuredLane, VendorIdentity,
    };

    use super::*;

    const NATIVE_ONLY: NativeFacadeDescriptor = NativeFacadeDescriptor {
        identity: VendorIdentity::new("codex"),
        command: "codex-native",
        aliases: &[],
        native: Some(Lane::new(
            true,
            NativeLane::new("codex", "codex", NativeAdapterId::new("codex-native")),
        )),
        structured: None,
    };

    #[test]
    fn only_the_crate_private_authorized_value_can_form_native_provenance() {
        let descriptors = [NATIVE_ONLY];
        let registry = NativeFacadeRegistry::new(&descriptors).unwrap();
        let authorized = AuthorizedNativeFacade {
            selector: "codex-native",
        };

        let selection = select_native_facade(&registry, LaunchIntent::native(authorized)).unwrap();
        let native = selection.into_native().unwrap();

        assert_eq!(native.native_lane().agent_type().name, "codex-impl");
    }

    #[test]
    fn a_structured_origin_cannot_cross_into_a_native_only_lane() {
        let descriptors = [NATIVE_ONLY];
        let registry = NativeFacadeRegistry::new(&descriptors).unwrap();

        for origin in [
            StructuredLaunchOrigin::Cli,
            StructuredLaunchOrigin::RemoteMcp,
            StructuredLaunchOrigin::RemoteAcp,
            StructuredLaunchOrigin::Subagent,
        ] {
            assert!(
                select_native_facade(&registry, LaunchIntent::structured("codex-native", origin))
                    .is_none()
            );
        }
    }

    #[test]
    fn a_native_origin_cannot_cross_into_a_structured_only_lane() {
        let descriptor = NativeFacadeDescriptor {
            native: None,
            structured: Some(Lane::new(
                true,
                StructuredLane::new(
                    StructuredAgentIdentity::new("codex-acp", 1),
                    StructuredControl::Acp,
                    StructuredAdapterId::new("codex-structured"),
                ),
            )),
            ..NATIVE_ONLY
        };
        let descriptors = [descriptor];
        let registry = NativeFacadeRegistry::new(&descriptors).unwrap();
        let authorized = AuthorizedNativeFacade {
            selector: "codex-native",
        };

        assert!(select_native_facade(&registry, LaunchIntent::native(authorized)).is_none());
    }

    #[test]
    fn disabled_native_policy_cannot_produce_a_selected_native_value() {
        let descriptor = NativeFacadeDescriptor {
            native: Some(NativeFacadeNativeLane::new(
                false,
                NativeLane::new("codex", "codex", NativeAdapterId::new("codex-native")),
            )),
            ..NATIVE_ONLY
        };
        let descriptors = [descriptor];
        let registry = NativeFacadeRegistry::new(&descriptors).unwrap();
        let authorized = AuthorizedNativeFacade {
            selector: "codex-native",
        };

        assert!(select_native_facade(&registry, LaunchIntent::native(authorized)).is_none());
    }
}
