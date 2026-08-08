//! §3.3's capability model — two-stage resolution, with the surface as a ceiling.
//!
//! > *"**Static**, keyed `(harness, harness_version, surfaces)`, produced by `marion doctor` and
//! > cached. […] **Refined** at session open where a handshake exists (ACP `initialize`,
//! > app-server capability reads), **narrowing — never widening** — the static set."*
//!
//! [`static_caps`] is stage one and has the signature §3.3 writes down. Stage two is
//! [`Capabilities::meet`] applied to whatever a handshake reports; [`crate::acp`] is the first
//! handshake that exists in this tree.
//!
//! # The ceiling is structural, not a convention
//!
//! §3.3: *"`ExecutionSurfaces` sets the ceiling; caps may only sit at or below it."* That could
//! have been a rule each table entry was trusted to obey. It is not: [`static_caps`] is
//! [`advertised`] **met** with [`Capabilities::ceiling`], so a table entry that over-claims is
//! clipped rather than believed. §9's M1 note is the case that makes this load-bearing rather than
//! decorative:
//!
//! > *"`codex exec resume [SESSION_ID] [PROMPT]` exists on 0.146.0 and is exactly `continue_()` +
//! > `prompt()`, but §3.4's `LaunchOnly` + `ProtocolEvents` derivation makes `continue_`
//! > `Unsupported` on the degenerate `ControlPlane` […] so `static_caps` returns `false` here
//! > correctly. **`marion doctor` keys on `(harness, version, surfaces)`, the same key
//! > `static_caps` uses**, so it never publishes "codex cannot resume"; it publishes "codex *on
//! > this surface* cannot"."*
//!
//! So [`advertised`] answers *can this software do it*, [`Capabilities::ceiling`] answers *can this
//! surface carry it*, and only their meet is publishable. Both halves are asserted below, and the
//! codex `resume` row is the one that would survive a `meet` that returned its left operand.
//!
//! # What is deliberately not a field
//!
//! §3.3 lists **ten** fields and states the exclusion as a rule: *"there is no `structured_events`
//! or `native_tui` capability, because those are already `ExecutionSurfaces` facts (`observations`
//! and `display`). A field that restates the surface is a second source of truth, and rev 2 had
//! exactly that bug."*
//!
//! Spike S20 produced the live temptation. `gemini --acp` advertises `promptCapabilities.audio` and
//! `opencode acp` does not — a real, measured difference between two agents behind one adapter. It
//! gets **no field here**: prompt content types are not among §3.3's ten, and recording a
//! difference is not claiming a capability for it. `the_struct_is_exactly_section_3_3s_ten_fields`
//! is what stops the next reader of that fixture from adding an eleventh.

use serde::{Deserialize, Serialize};

use marion_core::harness::Harness;

use crate::surfaces::{ControlTransport, ExecutionSurfaces, ObservationSource};

/// §3.3's struct, field for field.
///
/// `Serialize` is not incidental: `marion doctor` prints this, and
/// [`Capabilities::FIELDS`] is checked against the serialized key set, so the wire names and the
/// section's names cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// Mid-flight input into a running turn.
    pub steer: bool,
    /// Cancel a running turn.
    pub interrupt: bool,
    /// Branch a session.
    pub fork: bool,
    /// Continue a finished session (`continue_`).
    pub resume: bool,
    /// Replay history to rebuild a view.
    pub view: bool,
    /// Routes permission requests to marion. §3.3 is emphatic that this says only that a request
    /// *reaches* marion, never what marion does with it — marion denies (§11 item 22).
    pub permissions: bool,
    /// Routes structured input requests to marion. Same caveat as [`Self::permissions`].
    pub elicitation: bool,
    /// Change model mid-session.
    pub set_model: bool,
    /// Token-level streaming, not just whole messages.
    pub token_deltas: bool,
    /// Reports token/cost accounting.
    pub usage: bool,
}

impl Capabilities {
    /// §3.3's ten field names, in the section's order. The pin that keeps the struct at ten.
    pub const FIELDS: [&'static str; 10] = [
        "steer",
        "interrupt",
        "fork",
        "resume",
        "view",
        "permissions",
        "elicitation",
        "set_model",
        "token_deltas",
        "usage",
    ];

    /// Nothing claimed. The only honest starting point: §3.3's rule is *degrade visibly*, and an
    /// unmeasured capability rendered as available is the failure that rule exists to prevent.
    pub const NONE: Self = Self {
        steer: false,
        interrupt: false,
        fork: false,
        resume: false,
        view: false,
        permissions: false,
        elicitation: false,
        set_model: false,
        token_deltas: false,
        usage: false,
    };

    /// Every field true. **Not a claim about any harness** — it exists so a ceiling or a handshake
    /// can be applied to an unconstrained base in a test, and so [`Self::meet`] has a left identity
    /// to be checked against.
    pub const ALL: Self = Self {
        steer: true,
        interrupt: true,
        fork: true,
        resume: true,
        view: true,
        permissions: true,
        elicitation: true,
        set_model: true,
        token_deltas: true,
        usage: true,
    };

    /// Field-wise `&&`. The **only** way two capability sets combine, in either stage: a static
    /// table met with a surface ceiling, or a static set met with a handshake's answer. §3.3
    /// forbids widening in stage two, and a meet cannot widen — which is why refinement is spelled
    /// as a meet rather than as a replacement.
    #[must_use]
    pub fn meet(self, other: Self) -> Self {
        Self {
            steer: self.steer && other.steer,
            interrupt: self.interrupt && other.interrupt,
            fork: self.fork && other.fork,
            resume: self.resume && other.resume,
            view: self.view && other.view,
            permissions: self.permissions && other.permissions,
            elicitation: self.elicitation && other.elicitation,
            set_model: self.set_model && other.set_model,
            token_deltas: self.token_deltas && other.token_deltas,
            usage: self.usage && other.usage,
        }
    }

    /// Is every field of `self` at or below the matching field of `bound`? §3.3's ordering, as a
    /// predicate an assertion can name.
    pub fn is_at_or_below(&self, bound: &Self) -> bool {
        self.meet(*bound) == *self
    }

    /// The fields that are true, in [`Self::FIELDS`] order. What a `doctor` row prints.
    pub fn granted(&self) -> Vec<&'static str> {
        let flags = [
            self.steer,
            self.interrupt,
            self.fork,
            self.resume,
            self.view,
            self.permissions,
            self.elicitation,
            self.set_model,
            self.token_deltas,
            self.usage,
        ];
        Self::FIELDS
            .iter()
            .zip(flags)
            .filter_map(|(name, on)| on.then_some(*name))
            .collect()
    }

    /// **§3.3's ceiling** — what these surfaces can carry, regardless of harness.
    ///
    /// Derived from the three axes and from nothing else, so it moves when §3.4's derivation moves
    /// and never independently of it:
    ///
    /// * **A channel that can address the session** — `Typed(_)`. §3.4 says `TerminalInput` means
    ///   marion *"can write, but cannot address a turn"*, so `steer`, `fork`, `resume`, `set_model`
    ///   and the two routing fields need a typed plane. `permissions` and `elicitation` are
    ///   *routing* fields: a request only reaches marion over a channel marion can answer on, and a
    ///   pty node's permission prompt is drawn on a screen instead.
    /// * **Any channel at all** — `Typed(_)` or `TerminalInput`. `interrupt` needs only to reach
    ///   the running process, and S1/S11 measured a real interrupt protocol over a pty. `LaunchOnly`
    ///   has *"no channel afterwards"*, so it caps `interrupt` at false; killing a process is not
    ///   cancelling a turn.
    /// * **A structured stream** — `ProtocolEvents`. `token_deltas` is token-level streaming, which
    ///   `TerminalBytes` cannot supply: §3.4 calls those *"raw bytes off the pty, with no structure
    ///   marion can rely on"*.
    /// * **A replayable record** — `ProtocolEvents` or `TranscriptRecords`. `view` rebuilds history
    ///   and `usage` accounts for it; both need something durable that is not a byte grid.
    ///
    /// The match on [`ControlTransport`] is deliberately total: a fifth transport must decide these
    /// fields rather than inherit somebody's default.
    pub fn ceiling(s: &ExecutionSurfaces) -> Self {
        let addressable = match s.control {
            ControlTransport::Typed(_) => true,
            ControlTransport::TerminalInput | ControlTransport::LaunchOnly => false,
        };
        let any_channel = match s.control {
            ControlTransport::Typed(_) | ControlTransport::TerminalInput => true,
            ControlTransport::LaunchOnly => false,
        };
        let structured = s.observations.contains(&ObservationSource::ProtocolEvents);
        let replayable = structured
            || s.observations
                .contains(&ObservationSource::TranscriptRecords);
        Self {
            steer: addressable,
            interrupt: any_channel,
            fork: addressable,
            resume: addressable,
            view: replayable,
            permissions: addressable,
            elicitation: addressable,
            set_model: addressable,
            token_deltas: structured,
            usage: replayable,
        }
    }
}

/// **What the harness's software claims, before any surface clips it** — keyed `(harness,
/// version)`, the first two thirds of §3.3's key.
///
/// Every `true` below names the measurement behind it. Every field not named is `false`, and
/// `false` here means *marion has not measured it*, not *the harness cannot*. That asymmetry is
/// deliberate and it is §3.3's *degrade visibly*: an unmeasured capability rendered as available
/// greys in an action that will then fail, while an unmeasured one rendered as unavailable merely
/// understates a tool. `marion doctor`'s notes carry the difference in prose, because ten bools
/// cannot.
///
/// `version` is **load-bearing, not decoration.** §9 records `codex exec resume` as measured *"on
/// 0.146.0"*; marion has measured nothing before it and so claims nothing before it. A caller
/// passing an unparseable version gets the unmeasured answer rather than the optimistic one.
pub fn advertised(harness: Harness, version: &str) -> Capabilities {
    match harness {
        // S9 measured a real `can_use_tool` round-trip: a permission request from a headless
        // `claude -p` reaches marion and is answered (§9's M1 criterion-7 ledger, item 14).
        // S1 and S11 measured the interrupt protocol, S11 byte-for-byte over a real pty.
        Harness::ClaudeCode => Capabilities {
            interrupt: true,
            permissions: true,
            ..Capabilities::NONE
        },
        // §9: "`codex exec resume [SESSION_ID] [PROMPT]` exists on 0.146.0 and is exactly
        // `continue_()` + `prompt()`". That is a claim about the *binary*; the surface it is asked
        // for on is what decides whether it is publishable, and on `codex exec --json` it is not.
        Harness::Codex => Capabilities {
            resume: at_least(version, (0, 146, 0)),
            ..Capabilities::NONE
        },
        // Nothing measured. S12 measured 0.53.0 rewriting an explicit `-m`, and MILESTONES records
        // `gemini -p` refused by the vendor on this machine, so no capability has been observed to
        // work — including through ACP, where S20 found `session/new` refused outright.
        Harness::Gemini => Capabilities::NONE,
        // Nothing measured through the surface this adapter uses. S20 measured `opencode acp`
        // advertising `sessionCapabilities {close, fork, list, resume}`, but that is the *ACP*
        // surface's handshake, not `opencode run --pure --format json`, and §3.3 keys on surfaces
        // precisely so one cannot be read as the other. See `crate::acp` for where S20's answer is
        // actually consumed.
        Harness::OpenCode => Capabilities::NONE,
        // **The one row where `advertised` describes a protocol rather than a program**, because
        // §5.2's `acp` adapter serves many agents and the version here is not even readable until
        // one of them has answered `initialize`. So `version` is deliberately unused: it keys the
        // *agent*, and the agent is stage two's business.
        //
        // Five of the ten, and the five splits are three different arguments:
        //
        // * `token_deltas`, `usage` — **measured, S21.** A real `opencode acp` turn streamed
        //   `agent_message_chunk` frames one token at a time (`"The"`, `" user"`, `" wants"`) and
        //   emitted a `usage_update` with `used`/`size`/`cost`, and its `session/prompt` response
        //   carried a `usage` object. Both are in `tests/fixtures/s21/`.
        // * `fork`, `resume`, `view` — **the protocol has them and the handshake decides.** ACP v1
        //   defines `session/load` and advertises `sessionCapabilities`, and §3.3 makes ACP the
        //   worked example of a static set *refined* at session open. A `false` here would be
        //   final: [`Capabilities::meet`] cannot widen, so `opencode acp`'s advertised `fork` could
        //   never be published and M5's *"differing capabilities"* would have nothing to differ on.
        //   That is the one place this table states a protocol's shape rather than a measurement,
        //   and it is stated only because the very next stage narrows it per agent.
        // * `steer`, `interrupt`, `permissions`, `elicitation`, `set_model` — **not measured.** ACP
        //   defines all five (`session/prompt` mid-turn, `session/cancel`,
        //   `session/request_permission`, `session/request_input`, the `model` `configOption` S21
        //   saw in a `session/new` result), and marion has driven none of them against a live
        //   agent. §3.3's *degrade visibly*: a `false` here is "marion has not measured it", and
        //   the honest cost is an understated tool rather than a greyed-in action that then fails.
        Harness::Acp => Capabilities {
            fork: true,
            resume: true,
            view: true,
            token_deltas: true,
            usage: true,
            ..Capabilities::NONE
        },
    }
}

/// §3.3's stage one, with the section's own signature.
///
/// `advertised(harness, version)` met with `ceiling(surfaces)` — the meet is the whole point, and
/// `codex_can_resume_and_its_surface_is_what_says_it_cannot` is the row that proves it runs.
pub fn static_caps(harness: Harness, version: &str, s: &ExecutionSurfaces) -> Capabilities {
    advertised(harness, version).meet(Capabilities::ceiling(s))
}

/// `major.minor.patch` at or after `floor`. Anything that does not parse is **not** at or after it:
/// a version marion cannot read is a version marion has not measured.
fn at_least(version: &str, floor: (u64, u64, u64)) -> bool {
    let mut parts = version.trim().split('.');
    let mut next = || parts.next().and_then(|p| p.trim().parse::<u64>().ok());
    match (next(), next(), next()) {
        (Some(a), Some(b), Some(c)) => (a, b, c) >= floor,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::adapter_for;
    use crate::surfaces::TypedKind;
    use std::collections::BTreeSet;

    /// **§3.3 lists ten fields, and this is what keeps it at ten.**
    ///
    /// The live temptation is S20's: `gemini --acp` advertises `promptCapabilities.audio` and
    /// `opencode acp` does not. It is a real measured difference and it is *not* a capability —
    /// prompt content types are not among the ten, and §3.3's stated reason for the exclusions is
    /// that a field restating a surface fact is a second source of truth. Adding `audio`, or
    /// re-adding rev 2's `structured_events` / `native_tui`, fails here.
    #[test]
    fn the_struct_is_exactly_section_3_3s_ten_fields() {
        let json = serde_json::to_value(Capabilities::NONE).unwrap();
        let got: BTreeSet<String> = json
            .as_object()
            .expect("a struct")
            .keys()
            .cloned()
            .collect();
        let want: BTreeSet<String> = Capabilities::FIELDS.iter().map(|s| s.to_string()).collect();
        assert_eq!(got, want, "the serialized shape and FIELDS must agree");
        assert_eq!(got.len(), 10, "§3.3 lists ten");
        for absent in ["structured_events", "native_tui", "audio", "image"] {
            assert!(
                !got.contains(absent),
                "`{absent}` restates a surface or a prompt content type; §3.3 excludes it"
            );
        }
    }

    /// §9's M1 sentence, as an executable row — and the assertion that would survive a
    /// [`Capabilities::meet`] that returned its left operand.
    #[test]
    fn codex_can_resume_and_its_surface_is_what_says_it_cannot() {
        let v = "0.147.0";
        assert!(
            advertised(Harness::Codex, v).resume,
            "§9: `codex exec resume` exists on 0.146.0 and is `continue_()` + `prompt()`"
        );

        let shipped = adapter_for(Harness::Codex).unwrap().surfaces();
        assert!(
            !static_caps(Harness::Codex, v, &shipped).resume,
            "on `codex exec --json` the ceiling clips it: `marion doctor` publishes \"codex on \
             this surface cannot\", never \"codex cannot\""
        );

        // And the other half of the same sentence — "choosing the app-server surface lifts the
        // ceiling" — so this cannot pass by clipping `resume` everywhere.
        let app_server = ExecutionSurfaces::headless(TypedKind::AppServer);
        assert!(
            static_caps(Harness::Codex, v, &app_server).resume,
            "a typed plane can carry `continue_`, so the same binary publishes it there"
        );
    }

    /// **The ACP row of `advertised`, field by field, with the reason each `true` is a `true`.**
    ///
    /// Five entries and three different justifications, and they must not be collapsed. Asserting
    /// the whole struct in one `assert_eq!` would pass equally well for a row that had them right
    /// for the wrong reasons, so each is named — and the five `false`s are named too, because
    /// §3.3's *degrade visibly* makes an unmeasured capability's `false` a claim in its own right.
    #[test]
    fn the_acp_row_claims_only_what_the_protocol_has_or_s21_measured() {
        let a = advertised(Harness::Acp, "irrelevant");
        // Measured live in S21 (`tests/fixtures/s21/`): per-token `agent_message_chunk` frames, and
        // a `usage_update` frame plus a `usage` object on the `session/prompt` response.
        assert!(a.token_deltas && a.usage);
        // Stated as the protocol's shape **so stage two can narrow them**. A `false` here would be
        // final — a meet cannot widen — and `opencode acp`'s advertised `fork` could then never be
        // published, which is M5's second clause with nothing to report.
        assert!(a.fork && a.resume && a.view);
        // Defined by ACP, driven against no live agent, therefore not claimed.
        for (name, on) in [
            ("steer", a.steer),
            ("interrupt", a.interrupt),
            ("permissions", a.permissions),
            ("elicitation", a.elicitation),
            ("set_model", a.set_model),
        ] {
            assert!(!on, "`{name}` has not been driven against a live ACP agent");
        }
        assert_eq!(
            a.granted(),
            vec!["fork", "resume", "view", "token_deltas", "usage"]
        );
        // The version is genuinely not part of this key: for ACP the agent supplies its own
        // identity in the handshake, so `advertised` cannot look one up and must not pretend to.
        for v in ["", "0.0.1", "9.9.9", "OpenCode 1.17.3"] {
            assert_eq!(advertised(Harness::Acp, v), a, "{v:?}");
        }
    }

    /// The version is the middle third of §3.3's key. If it were decoration, both rows below would
    /// be the same.
    #[test]
    fn the_version_is_part_of_the_key_and_not_decoration() {
        let typed = ExecutionSurfaces::headless(TypedKind::AppServer);
        assert!(!static_caps(Harness::Codex, "0.145.0", &typed).resume);
        assert!(static_caps(Harness::Codex, "0.146.0", &typed).resume);
        assert!(static_caps(Harness::Codex, "1.0.0", &typed).resume);
        for unreadable in ["", "unknown", "0.146", "v0.146.0", "0.146.0-rc.1"] {
            assert!(
                !advertised(Harness::Codex, unreadable).resume,
                "a version marion cannot read is a version marion has not measured: {unreadable:?}"
            );
        }
    }

    /// §3.3's ceiling rule over the whole key space marion can construct today. Not vacuous: the
    /// codex row above exhibits a clip, and the count below asserts one happens here too.
    #[test]
    fn no_capability_is_ever_published_above_its_surfaces_ceiling() {
        let surfaces = [
            ExecutionSurfaces::shared(TypedKind::StreamJson),
            ExecutionSurfaces::headless(TypedKind::StreamJson),
            ExecutionSurfaces::headless(TypedKind::AppServer),
            ExecutionSurfaces::headless(TypedKind::Acp),
            ExecutionSurfaces::interactive(),
            ExecutionSurfaces::opaque(),
            ExecutionSurfaces::launch_only_with_protocol_events(),
        ];
        let mut clipped = 0;
        for h in Harness::ALL {
            for s in &surfaces {
                let got = static_caps(h, "9.9.9", s);
                let ceiling = Capabilities::ceiling(s);
                assert!(
                    got.is_at_or_below(&ceiling),
                    "{h} on {s:?} published {:?} above its ceiling {:?}",
                    got.granted(),
                    ceiling.granted()
                );
                if got != advertised(h, "9.9.9") {
                    clipped += 1;
                }
            }
        }
        assert!(
            clipped > 0,
            "no row was clipped, so the meet in `static_caps` is untested by this assertion"
        );
    }

    /// The surfaces every shipped adapter actually declares, keyed as §3.3 keys — so this is the
    /// table `marion doctor` publishes today, not a hypothetical one.
    #[test]
    fn every_shipped_adapters_surface_caps_what_it_publishes() {
        for h in Harness::ALL {
            let a = adapter_for(h).unwrap();
            let s = a.surfaces();
            let got = static_caps(h, "9.9.9", &s);
            assert!(got.is_at_or_below(&Capabilities::ceiling(&s)), "{h}");
            if matches!(s.control, ControlTransport::LaunchOnly) {
                assert_eq!(
                    got.granted(),
                    Vec::<&str>::new(),
                    "{h} runs `LaunchOnly`, which has no channel after argv, and marion has \
                     measured no observation-only capability on it"
                );
            }
        }
        // claude-code is the one shipped adapter with a typed plane, so it is the one row that is
        // not all-false. If it went all-false the assertion above would pass vacuously.
        let cc = adapter_for(Harness::ClaudeCode).unwrap().surfaces();
        assert_eq!(
            static_caps(Harness::ClaudeCode, "2.1.223", &cc).granted(),
            vec!["interrupt", "permissions"],
            "S1/S11 measured the interrupt protocol and S9 a real `can_use_tool` round-trip"
        );
    }

    /// `LaunchOnly` is §3.4's "no channel afterwards", and every control capability follows from
    /// that one fact rather than from a per-harness opinion.
    #[test]
    fn a_launch_only_surface_ceilings_every_control_capability_at_false() {
        let c = Capabilities::ceiling(&ExecutionSurfaces::launch_only_with_protocol_events());
        for (name, on) in [
            ("steer", c.steer),
            ("interrupt", c.interrupt),
            ("fork", c.fork),
            ("resume", c.resume),
            ("permissions", c.permissions),
            ("elicitation", c.elicitation),
            ("set_model", c.set_model),
        ] {
            assert!(!on, "`{name}` needs a channel and `LaunchOnly` has none");
        }
        // Its `ProtocolEvents` still buys the read-only three — otherwise this would be indist-
        // inguishable from a ceiling of all-false.
        assert_eq!(c.granted(), vec!["view", "token_deltas", "usage"]);
    }

    /// A pty is a display device (principle 3), not a structured stream — but it *is* a channel,
    /// which is what separates `interrupt` from `steer` here.
    #[test]
    fn a_pty_only_surface_can_be_interrupted_and_nothing_else() {
        let c = Capabilities::ceiling(&ExecutionSurfaces::opaque());
        assert_eq!(
            c.granted(),
            vec!["interrupt"],
            "S1/S11 measured a real interrupt protocol over a pty; `TerminalBytes` supplies no \
             history to view and no tokens to count"
        );
        // `interactive` adds `TranscriptRecords`, and that is the only difference.
        let i = Capabilities::ceiling(&ExecutionSurfaces::interactive());
        assert_eq!(i.granted(), vec!["interrupt", "view", "usage"]);
        assert!(
            !i.token_deltas,
            "transcript records are whole messages, not token deltas"
        );
    }

    /// A meet is a meet: commutative, idempotent, and never widening. §3.3's stage two is spelled
    /// as one so that "narrowing, never widening" is a property of the operation rather than a
    /// rule a refiner is trusted to follow.
    #[test]
    fn a_meet_narrows_and_can_never_widen() {
        assert_eq!(
            Capabilities::ALL.meet(Capabilities::NONE),
            Capabilities::NONE
        );
        assert_eq!(
            Capabilities::NONE.meet(Capabilities::ALL),
            Capabilities::NONE
        );
        assert_eq!(Capabilities::ALL.meet(Capabilities::ALL), Capabilities::ALL);

        // A mixed pair, so an implementation returning either operand unchanged fails.
        let a = Capabilities {
            fork: true,
            resume: true,
            ..Capabilities::NONE
        };
        let b = Capabilities {
            resume: true,
            usage: true,
            ..Capabilities::NONE
        };
        let want = Capabilities {
            resume: true,
            ..Capabilities::NONE
        };
        assert_eq!(a.meet(b), want);
        assert_eq!(b.meet(a), want, "commutative");
        assert_eq!(a.meet(b).meet(b), want, "idempotent");
        for got in [a.meet(b), b.meet(a)] {
            assert!(got.is_at_or_below(&a) && got.is_at_or_below(&b));
        }
    }

    /// `granted` is what a `doctor` row prints, so a field silently missing from it would publish a
    /// capability as absent while the struct says otherwise.
    #[test]
    fn granted_names_every_field_and_in_section_order() {
        assert_eq!(
            Capabilities::ALL.granted(),
            Capabilities::FIELDS.to_vec(),
            "every field must be reachable through `granted`, in §3.3's order"
        );
        assert!(Capabilities::NONE.granted().is_empty());
        // And each field individually, so a mis-zipped pair cannot hide behind the all-true row:
        // swapping any two entries of `granted`'s `flags` array leaves the all-true and all-false
        // rows identical and breaks exactly one row here.
        type Setter = (&'static str, fn(&mut Capabilities));
        let setters: [Setter; 10] = [
            ("steer", |c| c.steer = true),
            ("interrupt", |c| c.interrupt = true),
            ("fork", |c| c.fork = true),
            ("resume", |c| c.resume = true),
            ("view", |c| c.view = true),
            ("permissions", |c| c.permissions = true),
            ("elicitation", |c| c.elicitation = true),
            ("set_model", |c| c.set_model = true),
            ("token_deltas", |c| c.token_deltas = true),
            ("usage", |c| c.usage = true),
        ];
        for (name, set) in setters {
            let mut one = Capabilities::NONE;
            set(&mut one);
            assert_eq!(
                one.granted(),
                vec![name],
                "the `{name}` field maps to `{name}`"
            );
        }
    }

    #[test]
    fn capabilities_round_trip_through_their_wire_shape() {
        let c = Capabilities {
            steer: true,
            usage: true,
            ..Capabilities::NONE
        };
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Capabilities>(&s).unwrap(), c);
        // `deny_unknown_fields`, so a harness inventing an eleventh cannot be read as a tenth.
        assert!(serde_json::from_str::<Capabilities>(r#"{"audio":true}"#).is_err());
    }
}
