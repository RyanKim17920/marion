//! Execution surfaces (design §3.4).
//!
//! > *"The four familiar mode names are **presets over a cross-product of three independent
//! > properties** […] Encoding them as a flat enum makes valid combinations unrepresentable."*
//!
//! The proof that the flat enum would have been wrong is already in the tree: marion's own M1
//! child is `codex exec --json`, which is `LaunchOnly` control with `ProtocolEvents` observation
//! and **no display** — a legitimate combination outside all four presets (§3.4's derivation
//! table, §9). So the presets are constructors here, never the type.
//!
//! `observations` is a `BTreeSet`, not the design's illustrative `EnumSet`: the sketch's
//! `enum-set` crate would be a workspace dependency bought for one field, and a `BTreeSet` over
//! four variants gives the same set semantics plus a deterministic order for comparison.

use std::collections::BTreeSet;

/// How marion puts input *into* a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ControlTransport {
    /// A structured protocol marion speaks: prompt, steer, interrupt, resume.
    Typed(TypedKind),
    /// Keystrokes into a pty. marion can write, but cannot address a turn.
    TerminalInput,
    /// The prompt rides argv and there is no channel afterwards. `codex exec` is this.
    LaunchOnly,
}

/// Which typed protocol, where the transport is `Typed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TypedKind {
    /// Claude Code's `--input-format stream-json` / `--output-format stream-json` pipe pair.
    StreamJson,
    /// Codex's app-server (M4).
    AppServer,
    /// Agent Client Protocol (M5).
    Acp,
}

/// Where the node's output is *rendered*. Not where marion reads it from — that is
/// [`ObservationSource`], and conflating the two was rev 2's bug (§3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DisplaySurface {
    /// marion owns a pty and a VT grid; `DisplayPlane` is implemented iff this holds.
    NativePty,
    /// marion renders structured events itself.
    StructuredUi,
    /// Nothing is displayed. A headless child marion only collects from.
    None,
}

/// Where marion reads a node's activity from. Several at once is normal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObservationSource {
    /// A structured stream the harness emits: `stream-json` frames, `exec --json` JSONL.
    ProtocolEvents,
    /// Records the harness writes to disk as it goes.
    TranscriptRecords,
    /// Raw bytes off the pty, with no structure marion can rely on.
    TerminalBytes,
}

/// The three axes, independent. §3.4's four presets are the constructors below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionSurfaces {
    pub control: ControlTransport,
    pub display: DisplaySurface,
    pub observations: BTreeSet<ObservationSource>,
}

impl ExecutionSurfaces {
    /// The general constructor. Every preset below is one call to this, and so is the codex
    /// combination that is not a preset — which is the whole point of §3.4.
    pub fn new(
        control: ControlTransport,
        display: DisplaySurface,
        observations: impl IntoIterator<Item = ObservationSource>,
    ) -> Self {
        Self {
            control,
            display,
            observations: observations.into_iter().collect(),
        }
    }

    /// Preset `shared`: typed control over a pty marion also owns. Preferred wherever it exists.
    pub fn shared(kind: TypedKind) -> Self {
        Self::new(
            ControlTransport::Typed(kind),
            DisplaySurface::NativePty,
            [
                ObservationSource::ProtocolEvents,
                ObservationSource::TerminalBytes,
            ],
        )
    }

    /// Preset `headless`: typed control over pipes, no pty. M1's root is this (§6.4).
    pub fn headless(kind: TypedKind) -> Self {
        Self::new(
            ControlTransport::Typed(kind),
            DisplaySurface::StructuredUi,
            [ObservationSource::ProtocolEvents],
        )
    }

    /// Preset `interactive`: a pty marion types into, observed through the harness's transcript.
    pub fn interactive() -> Self {
        Self::new(
            ControlTransport::TerminalInput,
            DisplaySurface::NativePty,
            [
                ObservationSource::TranscriptRecords,
                ObservationSource::TerminalBytes,
            ],
        )
    }

    /// Preset `opaque`: the universal floor — a pty and nothing else.
    pub fn opaque() -> Self {
        Self::new(
            ControlTransport::TerminalInput,
            DisplaySurface::NativePty,
            [ObservationSource::TerminalBytes],
        )
    }

    /// **Not a preset.** `codex exec --json`: spawn, stream, read the terminal result. No steer,
    /// no resume, no pty (§3.4's derivation table, §9). This is M1's child, and the reason
    /// `ExecutionSurfaces` is three axes rather than one enum.
    pub fn launch_only_with_protocol_events() -> Self {
        Self::new(
            ControlTransport::LaunchOnly,
            DisplaySurface::None,
            [ObservationSource::ProtocolEvents],
        )
    }

    /// §3.4: `DisplayPlane` is implemented iff `display == NativePty`.
    pub fn has_display_plane(&self) -> bool {
        self.display == DisplaySurface::NativePty
    }

    /// §3.4: a *typed* `ControlPlane` — the full trait — iff `control == Typed(_)`.
    pub fn has_typed_control_plane(&self) -> bool {
        matches!(self.control, ControlTransport::Typed(_))
    }

    /// §3.4: a **degenerate** `ControlPlane` — read-only `events()` — when control is not typed
    /// but some source other than `TerminalBytes` exists. This is what M1's codex child gets.
    pub fn has_degenerate_control_plane(&self) -> bool {
        !self.has_typed_control_plane()
            && self
                .observations
                .iter()
                .any(|o| *o != ObservationSource::TerminalBytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_presets_are_distinct_points_in_the_cross_product() {
        let all = [
            ExecutionSurfaces::shared(TypedKind::StreamJson),
            ExecutionSurfaces::headless(TypedKind::StreamJson),
            ExecutionSurfaces::interactive(),
            ExecutionSurfaces::opaque(),
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                assert_eq!(i == j, a == b, "presets {i} and {j}");
            }
        }
    }

    #[test]
    fn each_preset_matches_the_row_in_the_table() {
        let s = ExecutionSurfaces::shared(TypedKind::StreamJson);
        assert_eq!(s.control, ControlTransport::Typed(TypedKind::StreamJson));
        assert_eq!(s.display, DisplaySurface::NativePty);
        assert_eq!(s.observations.len(), 2);

        let h = ExecutionSurfaces::headless(TypedKind::StreamJson);
        assert_eq!(h.display, DisplaySurface::StructuredUi);
        assert_eq!(
            h.observations,
            BTreeSet::from([ObservationSource::ProtocolEvents])
        );

        let i = ExecutionSurfaces::interactive();
        assert_eq!(i.control, ControlTransport::TerminalInput);
        assert!(
            i.observations
                .contains(&ObservationSource::TranscriptRecords)
        );

        let o = ExecutionSurfaces::opaque();
        assert_eq!(
            o.observations,
            BTreeSet::from([ObservationSource::TerminalBytes])
        );
    }

    /// The combination the flat-enum model could not have expressed, and which marion's own M1
    /// child *is*. If this stops being constructible, §3.4's argument has been undone.
    #[test]
    fn the_codex_child_is_expressible_and_is_not_one_of_the_presets() {
        let codex = ExecutionSurfaces::launch_only_with_protocol_events();
        assert_eq!(codex.control, ControlTransport::LaunchOnly);
        assert_eq!(codex.display, DisplaySurface::None);
        assert_eq!(
            codex.observations,
            BTreeSet::from([ObservationSource::ProtocolEvents])
        );
        for preset in [
            ExecutionSurfaces::shared(TypedKind::StreamJson),
            ExecutionSurfaces::headless(TypedKind::StreamJson),
            ExecutionSurfaces::interactive(),
            ExecutionSurfaces::opaque(),
        ] {
            assert_ne!(codex, preset, "it is a fifth point, not a fifth preset");
        }
    }

    #[test]
    fn planes_are_derived_from_the_axes_not_from_a_mode_name() {
        // §3.4's derivation table, read straight off the three axes.
        let codex = ExecutionSurfaces::launch_only_with_protocol_events();
        assert!(!codex.has_display_plane(), "no pty");
        assert!(!codex.has_typed_control_plane(), "LaunchOnly cannot steer");
        assert!(
            codex.has_degenerate_control_plane(),
            "its JSONL stream is an event source, so events() exists"
        );

        let opaque = ExecutionSurfaces::opaque();
        assert!(opaque.has_display_plane());
        assert!(
            !opaque.has_degenerate_control_plane(),
            "TerminalBytes alone is Payload::Raw read straight from the pty, not a ControlPlane"
        );

        let headless = ExecutionSurfaces::headless(TypedKind::StreamJson);
        assert!(headless.has_typed_control_plane());
        assert!(!headless.has_display_plane(), "headless runs over pipes");
    }

    #[test]
    fn observations_is_a_set_so_a_repeat_is_not_a_second_source() {
        let s = ExecutionSurfaces::new(
            ControlTransport::LaunchOnly,
            DisplaySurface::None,
            [
                ObservationSource::ProtocolEvents,
                ObservationSource::ProtocolEvents,
            ],
        );
        assert_eq!(s, ExecutionSurfaces::launch_only_with_protocol_events());
    }
}
