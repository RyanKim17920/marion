//! Mouse reports back to the node, in SGR-1006.
//!
//! # Why this is hand-written
//!
//! Encoding a mouse report is a dozen lines of arithmetic over a button byte. What makes it worth
//! its own module is the *gating*: a mouse report must be sent **only when the child asked for
//! one**, in the encoding the child asked for, and the child says so with the `?1000/1002/1003/1006`
//! modes that [`crate::sticky`] already tracks for the replay preamble. The same four booleans
//! answer both questions, so there is one source of truth and not two.
//!
//! # Why SGR-1006 and not X10
//!
//! The legacy X10 encoding (`ESC [ M Cb Cx Cy`) adds 32 to each coordinate and stuffs it in one
//! byte, so **column 224 is the last one it can express** — past that the byte wraps and the child
//! is told about a click somewhere else entirely. It also cannot report a button *release*
//! distinctly: every release is button 3, so a child cannot tell which button came up. SGR-1006
//! (`ESC [ < b ; x ; y M|m`) is decimal, unbounded, and distinguishes press (`M`) from release
//! (`m`).
//!
//! 224 columns is not hypothetical. §5.3's captures resize to 140 columns and a maximized terminal
//! on a wide display passes 224 routinely, so X10 would fail on real hardware, in a way that
//! presents as "clicks land in the wrong place when the window is wide" — a bug report nobody
//! would trace back to an encoding choice. `?1006` is in §5.3 L1221's required list, both Claude
//! captures enable it (measured, at byte 122 and 1955), and it is the only mode of the four that
//! changes the *encoding* rather than which events are reported.
//!
//! # What is deliberately not here
//!
//! No state machine. There is no drag tracker, no click-count timer, no double-click synthesis and
//! no button-held set. The child asked for motion events or it did not; marion forwards what the
//! user's terminal reported and lets the child do its own gesture recognition, which it must
//! already do to run under a real terminal.

use crate::sticky::Sticky;

/// Which button, in the sense SGR-1006's low two bits encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    /// Motion with no button held. Only meaningful under `?1003`.
    None,
    WheelUp,
    WheelDown,
}

/// What happened to the button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Press,
    Release,
    /// Motion, with or without a button held.
    Motion,
}

/// Keyboard modifiers, which SGR-1006 folds into the button code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// One mouse event from the user's terminal, in **zero-based** cells.
///
/// Zero-based because that is what every terminal library reports and what `ratatui` uses;
/// SGR-1006 is one-based on the wire, and [`encode`] is the single place that `+ 1` happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub button: Button,
    pub kind: Kind,
    pub col: u16,
    pub row: u16,
    pub mods: Mods,
}

/// SGR-1006's button code: the button in the low bits, modifiers and motion ORed on top.
fn code(ev: &MouseEvent) -> u32 {
    let mut b: u32 = match ev.button {
        Button::Left => 0,
        Button::Middle => 1,
        Button::Right => 2,
        Button::None => 3,
        // Wheel events set bit 6 (64) and use the low two bits as an index.
        Button::WheelUp => 64,
        Button::WheelDown => 65,
    };
    if ev.kind == Kind::Motion {
        b |= 32; // bit 5: this is a motion report, not a fresh press.
    }
    if ev.mods.shift {
        b |= 4;
    }
    if ev.mods.alt {
        b |= 8;
    }
    if ev.mods.ctrl {
        b |= 16;
    }
    b
}

/// Encode one event as `ESC [ < b ; x ; y M` (press/motion) or `… m` (release).
///
/// Coordinates are made one-based here and nowhere else.
pub fn encode(ev: &MouseEvent) -> Vec<u8> {
    let final_byte = if ev.kind == Kind::Release { 'm' } else { 'M' };
    format!(
        "\x1b[<{};{};{}{}",
        code(ev),
        ev.col as u32 + 1,
        ev.row as u32 + 1,
        final_byte
    )
    .into_bytes()
}

/// Encode `ev` **only if** the node's current modes ask for it.
///
/// Returns `None` when the child enabled no tracking, or enabled only press/release tracking and
/// this is a motion event. A client that sent reports unconditionally would inject `ESC[<…M` as
/// *text* into every harness that never asked for a mouse — Codex, on the measurements in
/// [`crate::sticky`], enables no mouse tracking at all — and the harness would paint the escape
/// as literal characters or treat it as a stray key.
///
/// **`?1006` gates the send, not just the format.** marion emits SGR and only SGR; a child that
/// asked for tracking without SGR is asking for an encoding this module deliberately does not
/// implement, and sending it SGR anyway would be worse than sending nothing — it would look like
/// a burst of garbage keystrokes rather than a missing feature. Both committed Claude captures
/// enable `?1006` alongside `?1000/1002/1003`, so this costs nothing on the harnesses that
/// actually want a mouse.
pub fn encode_for(ev: &MouseEvent, modes: &Sticky) -> Option<Vec<u8>> {
    if !modes.mouse_sgr {
        return None;
    }
    let tracking = modes.mouse_button || modes.mouse_drag || modes.mouse_motion;
    if !tracking {
        return None;
    }
    let is_motion = ev.kind == Kind::Motion;
    let held = ev.button != Button::None;
    let wanted = match (is_motion, held) {
        // A press or release: any tracking mode reports it.
        (false, _) => true,
        // Drag (motion with a button down): ?1002 or ?1003.
        (true, true) => modes.mouse_drag || modes.mouse_motion,
        // Bare motion: ?1003 only.
        (true, false) => modes.mouse_motion,
    };
    wanted.then(|| encode(ev))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            button: Button::Left,
            kind: Kind::Press,
            col,
            row,
            mods: Mods::default(),
        }
    }

    fn all_on() -> Sticky {
        Sticky {
            mouse_button: true,
            mouse_drag: true,
            mouse_motion: true,
            mouse_sgr: true,
            ..Sticky::initial(80, 24)
        }
    }

    fn s(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn a_left_press_is_sgr_with_one_based_coordinates() {
        assert_eq!(s(encode(&at(0, 0))), "\x1b[<0;1;1M");
        assert_eq!(s(encode(&at(9, 4))), "\x1b[<0;10;5M");
    }

    #[test]
    fn a_release_ends_in_lowercase_m_and_keeps_its_button() {
        let ev = MouseEvent {
            button: Button::Right,
            kind: Kind::Release,
            ..at(3, 3)
        };
        // X10 cannot express this at all: every release is button 3 there.
        assert_eq!(s(encode(&ev)), "\x1b[<2;4;4m");
    }

    #[test]
    fn the_three_buttons_take_the_low_two_bits() {
        for (b, want) in [(Button::Left, 0), (Button::Middle, 1), (Button::Right, 2)] {
            let ev = MouseEvent {
                button: b,
                ..at(0, 0)
            };
            assert_eq!(s(encode(&ev)), format!("\x1b[<{want};1;1M"));
        }
    }

    #[test]
    fn the_wheel_sets_bit_six() {
        let up = MouseEvent {
            button: Button::WheelUp,
            ..at(0, 0)
        };
        let down = MouseEvent {
            button: Button::WheelDown,
            ..at(0, 0)
        };
        assert_eq!(s(encode(&up)), "\x1b[<64;1;1M");
        assert_eq!(s(encode(&down)), "\x1b[<65;1;1M");
    }

    #[test]
    fn motion_sets_bit_five() {
        let ev = MouseEvent {
            kind: Kind::Motion,
            ..at(1, 1)
        };
        assert_eq!(s(encode(&ev)), "\x1b[<32;2;2M");
    }

    #[test]
    fn modifiers_or_into_the_button_code() {
        let ev = MouseEvent {
            mods: Mods {
                shift: true,
                alt: true,
                ctrl: true,
            },
            ..at(0, 0)
        };
        assert_eq!(s(encode(&ev)), "\x1b[<28;1;1M", "4 | 8 | 16");
    }

    /// The reason X10 is refused, as an assertion rather than a comment.
    #[test]
    fn a_column_past_the_x10_ceiling_is_reported_exactly() {
        // X10 encodes a coordinate as one byte offset by 32, so 224 is its last column and
        // anything beyond wraps into a different cell. A 300-column terminal is ordinary.
        let ev = at(299, 199);
        assert_eq!(s(encode(&ev)), "\x1b[<0;300;200M");
        // And the decisive property: two far-apart wide columns must not encode alike.
        assert_ne!(encode(&at(300, 0)), encode(&at(44, 0)));
    }

    #[test]
    fn no_report_is_sent_when_the_child_asked_for_no_mouse() {
        // Codex enables no mouse tracking at all; a report would land as stray input.
        let off = Sticky::initial(80, 24);
        assert_eq!(encode_for(&at(0, 0), &off), None);
    }

    #[test]
    fn no_report_is_sent_when_the_child_wants_tracking_without_sgr() {
        let modes = Sticky {
            mouse_button: true,
            mouse_sgr: false,
            ..Sticky::initial(80, 24)
        };
        assert_eq!(encode_for(&at(0, 0), &modes), None);
    }

    #[test]
    fn bare_motion_needs_1003_and_a_drag_needs_only_1002() {
        let motion = MouseEvent {
            button: Button::None,
            kind: Kind::Motion,
            ..at(0, 0)
        };
        let drag = MouseEvent {
            button: Button::Left,
            kind: Kind::Motion,
            ..at(0, 0)
        };

        let press_only = Sticky {
            mouse_button: true,
            mouse_sgr: true,
            ..Sticky::initial(80, 24)
        };
        assert_eq!(
            encode_for(&motion, &press_only),
            None,
            "?1000 reports no motion"
        );
        assert_eq!(
            encode_for(&drag, &press_only),
            None,
            "?1000 reports no drag either"
        );

        let with_drag = Sticky {
            mouse_drag: true,
            mouse_sgr: true,
            ..Sticky::initial(80, 24)
        };
        assert_eq!(
            encode_for(&motion, &with_drag),
            None,
            "?1002 is drag, not bare motion"
        );
        assert!(encode_for(&drag, &with_drag).is_some());

        let any = Sticky {
            mouse_motion: true,
            mouse_sgr: true,
            ..Sticky::initial(80, 24)
        };
        assert!(
            encode_for(&motion, &any).is_some(),
            "?1003 reports bare motion"
        );
    }

    #[test]
    fn a_press_is_reported_under_every_tracking_mode() {
        for modes in [
            Sticky {
                mouse_button: true,
                mouse_sgr: true,
                ..Sticky::initial(80, 24)
            },
            Sticky {
                mouse_drag: true,
                mouse_sgr: true,
                ..Sticky::initial(80, 24)
            },
            Sticky {
                mouse_motion: true,
                mouse_sgr: true,
                ..Sticky::initial(80, 24)
            },
        ] {
            assert!(encode_for(&at(2, 2), &modes).is_some());
        }
    }

    #[test]
    fn gating_does_not_alter_the_encoding() {
        let ev = at(7, 8);
        assert_eq!(encode_for(&ev, &all_on()).unwrap(), encode(&ev));
    }
}
