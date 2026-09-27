//! **What the node's terminal will do with bytes typed into it** — the facts a writer other than
//! the operator (turn delivery's paste, `crate::paste`) has to know before it types.
//!
//! Two of them are read off the node's own output. Whether it asked for **bracketed paste**
//! (`CSI ? 2004 h`): a paste into a terminal that did not ask for it arrives as typed keys, and a
//! newline inside the message then submits half of it; S31 measured codex turning an unbracketed
//! burst's CR into a newline, so marion never types a paste the node cannot tell from typing. And
//! whether it has **drawn its screen** since, and when it last wrote anything: copilot 1.0.83
//! turns bracketed paste on in its first write and then spends seconds (twenty, with nobody
//! answering its terminal queries) before it draws a thing, and a paste in that window is
//! discarded without a trace — so a first paste waits for text on the screen and a quiet moment.
//!
//! The other is read off the operator's own writes ([`Typing`]): when a person last typed into
//! this terminal, and whether what they typed has been submitted. A paste landing in the middle of
//! a half-typed line would be submitted *with* it — the operator's words and marion's as one turn.

use std::time::Instant;

use marion_harness::spec::BootDialog;

/// Longest parameter list kept for one private-mode CSI. Real mode sets are a handful of short
/// numbers; anything longer is not one marion needs to read, and a bound keeps a hostile stream
/// from growing this.
const MAX_PARAMS: usize = 64;

/// The DEC private mode for bracketed paste.
const BRACKETED_PASTE: &[u8] = b"2004";

/// Most screen text kept for recognising a boot dialog. The longest measured first screen with a
/// dialog carries about 1.5 KiB of text (copilot 1.0.83, banner and box included); a TUI that
/// keeps drawing past this without a key has moved past its dialog. Past the cap the oldest half
/// goes, so the cost per byte stays constant.
pub(crate) const SCREEN_TEXT_CAP: usize = 8192;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Scan {
    #[default]
    Ground,
    /// After `ESC`.
    Escape,
    /// After `ESC [`: a private marker may follow.
    CsiEntry,
    /// Inside `ESC [ ?`, collecting parameters.
    Private,
    /// Inside a CSI marion does not read; waiting for its final byte.
    Ignore,
    /// Inside a control string (OSC, DCS, APC, PM, SOS): its payload is not text on the screen.
    /// Ended by `BEL`, or by `ESC \` through [`Scan::Escape`].
    Str,
}

/// **DECSET/DECRST 2004, followed across reads, and whether text has been drawn since.** A small
/// state machine over the output stream — only the sequences that change the mode are recognised,
/// control strings are skipped whole, and a read boundary anywhere inside one is carried, because
/// a `read()` is not a frame (S11).
#[derive(Debug, Default)]
pub(crate) struct ModeScan {
    state: Scan,
    params: Vec<u8>,
    overflowed: bool,
    bracketed_paste: bool,
    drawn: bool,
    last_output: Option<Instant>,
    /// The row's boot dialogs this scan recognises ([`Self::showing`]).
    dialogs: &'static [BootDialog],
    /// The visible text written since anyone last typed, words joined by one space — what a
    /// dialog's needle is matched against. Kept from the first byte, because a TUI can draw its
    /// dialog before a paste driver is attached, and dropped for good at [`Self::boot_over`].
    screen: Vec<u8>,
    /// A space, a line break or a cursor move came after the last visible byte.
    gap: bool,
    /// Bit `i`: `dialogs[i]` was drawn since anyone last typed. Latched rather than re-read from
    /// [`Self::screen`], because a TUI that animates behind its dialog (codex's welcome art)
    /// writes past the cap while the dialog, drawn once, stays on screen.
    seen: u64,
    /// Visible text arrived since the last look for needles.
    fresh: bool,
    boot_over: bool,
}

impl ModeScan {
    /// Whether the node has asked for bracketed paste and not since withdrawn it.
    pub(crate) fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    /// Whether the node has written visible text — a byte that is neither a control, a space nor
    /// part of an escape sequence or control string — since it last turned bracketed paste on.
    pub(crate) fn drawn(&self) -> bool {
        self.drawn
    }

    /// When the node last wrote anything at all, `None` before its first byte.
    pub(crate) fn last_output(&self) -> Option<Instant> {
        self.last_output
    }

    /// Recognise `dialogs` on the screen from now on — over what was drawn already, too.
    pub(crate) fn watch(&mut self, dialogs: &'static [BootDialog]) {
        // One bit per dialog; a row lists two or three.
        self.dialogs = &dialogs[..dialogs.len().min(64)];
        self.seen = 0;
        self.look();
    }

    /// **The first of the watched dialogs on the screen**, in the row's order: its needle was
    /// drawn since anyone last typed. `None` once boot is over.
    pub(crate) fn showing(&self) -> Option<&'static BootDialog> {
        self.dialogs
            .iter()
            .enumerate()
            .find(|(i, _)| self.seen & (1 << i) != 0)
            .map(|(_, d)| d)
    }

    /// Latch every watched needle the kept text now holds.
    fn look(&mut self) {
        self.fresh = false;
        for (i, d) in self.dialogs.iter().enumerate() {
            let needle = d.needle.as_bytes();
            if self.seen & (1 << i) == 0
                && !needle.is_empty()
                && self.screen.windows(needle.len()).any(|w| w == needle)
            {
                self.seen |= 1 << i;
            }
        }
    }

    /// The operator typed: a key is what dismisses a dialog, and one that survives it redraws.
    pub(crate) fn operator_typed(&mut self) {
        self.screen.clear();
        self.gap = false;
        self.seen = 0;
    }

    /// Marion typed: as [`Self::operator_typed`], and the drawn screen is forgotten too, so a
    /// first paste after marion answered a dialog waits for the TUI to redraw.
    pub(crate) fn marion_typed(&mut self) {
        self.operator_typed();
        self.drawn = false;
    }

    /// The first paste landed: no dialog is looked for again, and no screen text is kept.
    pub(crate) fn boot_over(&mut self) {
        self.boot_over = true;
        self.dialogs = &[];
        self.seen = 0;
        self.screen = Vec::new();
    }

    #[cfg(test)]
    pub(crate) fn screen_len(&self) -> usize {
        self.screen.len()
    }

    fn visible(&mut self, b: u8) {
        if self.boot_over {
            return;
        }
        if self.screen.len() >= SCREEN_TEXT_CAP {
            // Every needle is far shorter than the half kept, so one still being drawn survives
            // the drain whole, and one already drawn is latched before it goes.
            self.look();
            self.screen.drain(..SCREEN_TEXT_CAP / 2);
        }
        self.fresh = true;
        if self.gap && !self.screen.is_empty() {
            self.screen.push(b' ');
        }
        self.gap = false;
        self.screen.push(b);
    }

    /// A CSI ended with `fin`: anything but a colour change moves the cursor or clears, which on
    /// the screen separates words (claude draws every space as `CSI n G`).
    fn csi_end(&mut self, fin: u8) {
        if fin != b'm' {
            self.gap = true;
        }
    }

    /// One chunk of the node's output, read at `now`.
    pub(crate) fn feed(&mut self, chunk: &[u8], now: Instant) {
        if !chunk.is_empty() {
            self.last_output = Some(now);
        }
        for &b in chunk {
            self.step(b);
        }
        if self.fresh && !self.dialogs.is_empty() {
            self.look();
        }
    }

    fn step(&mut self, b: u8) {
        // `ESC` restarts a sequence from anywhere, as a terminal's parser does; `CAN`/`SUB`
        // abandon one.
        if b == 0x1b {
            self.state = Scan::Escape;
            return;
        }
        if b == 0x18 || b == 0x1a {
            self.state = Scan::Ground;
            return;
        }
        self.state = match self.state {
            Scan::Ground => {
                // Text: printable ASCII past the space, or any byte of a UTF-8 sequence.
                if b > 0x20 && b != 0x7f {
                    self.drawn |= self.bracketed_paste;
                    self.visible(b);
                } else {
                    self.gap = true;
                }
                Scan::Ground
            }
            Scan::Escape => match b {
                b'[' => Scan::CsiEntry,
                b']' | b'P' | b'_' | b'^' | b'X' => Scan::Str,
                // RIS: a full reset, which clears every DEC private mode.
                b'c' => {
                    self.bracketed_paste = false;
                    self.drawn = false;
                    self.operator_typed();
                    Scan::Ground
                }
                _ => Scan::Ground,
            },
            Scan::CsiEntry => match b {
                b'?' => {
                    self.params.clear();
                    self.overflowed = false;
                    Scan::Private
                }
                0x40..=0x7e => {
                    self.csi_end(b);
                    Scan::Ground
                }
                _ => Scan::Ignore,
            },
            Scan::Private => match b {
                b'0'..=b'9' | b';' => {
                    if self.params.len() < MAX_PARAMS {
                        self.params.push(b);
                    } else {
                        self.overflowed = true;
                    }
                    Scan::Private
                }
                b'h' | b'l' if !self.overflowed => {
                    if self
                        .params
                        .split(|&c| c == b';')
                        .any(|p| p == BRACKETED_PASTE)
                    {
                        let on = b == b'h';
                        if on && !self.bracketed_paste {
                            // What counts is a screen drawn under this mode, not before it.
                            self.drawn = false;
                        }
                        self.bracketed_paste = on;
                    }
                    Scan::Ground
                }
                0x40..=0x7e => {
                    self.csi_end(b);
                    Scan::Ground
                }
                // An intermediate (`$` in DECRQM, `CSI ? 2004 $ p`) makes it another command.
                _ => Scan::Ignore,
            },
            Scan::Ignore => match b {
                0x40..=0x7e => {
                    self.csi_end(b);
                    Scan::Ground
                }
                _ => Scan::Ignore,
            },
            Scan::Str => match b {
                0x07 => Scan::Ground,
                _ => Scan::Str,
            },
        };
    }
}

/// **What a writer other than the operator needs to know before it types**, read together under
/// the host's write lock so no operator keystroke can land between the reading and the typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputState {
    /// The node asked for bracketed paste ([`ModeScan`]).
    pub bracketed_paste: bool,
    /// When the operator last typed a key — `None` if nobody has.
    pub last_operator_input: Option<Instant>,
    /// The operator's last key submitted (or nobody has typed): the composer is **presumed**
    /// empty. Presumed, because only the harness knows its composer; marion knows the keys.
    pub composer_empty: bool,
    /// The node has drawn text since it turned bracketed paste on ([`ModeScan::drawn`]).
    pub screen_drawn: bool,
    /// When the node last wrote anything — `None` before its first byte.
    pub last_output: Option<Instant>,
    /// The row's boot dialog drawn since anyone last typed, before the first paste
    /// ([`ModeScan::showing`]).
    pub boot_dialog: Option<&'static BootDialog>,
}

/// The operator's typing, as far as the host saw it. See [`InputState`].
#[derive(Debug)]
pub(crate) struct Typing {
    last_operator_input: Option<Instant>,
    composer_empty: bool,
}

impl Default for Typing {
    fn default() -> Self {
        Typing {
            last_operator_input: None,
            composer_empty: true,
        }
    }
}

impl Typing {
    pub(crate) fn last_operator_input(&self) -> Option<Instant> {
        self.last_operator_input
    }

    pub(crate) fn composer_empty(&self) -> bool {
        self.composer_empty
    }

    /// The operator wrote `bytes` at `now`. Reports the operator's terminal makes on its own —
    /// focus in/out, mouse, key releases — are not typing and change nothing. Otherwise the last
    /// key decides: an Enter submitted the line, anything else (an arrow can recall history into
    /// the composer) leaves it presumed non-empty.
    pub(crate) fn operator_wrote(&mut self, bytes: &[u8], now: Instant) {
        let mut last = None;
        for key in Keys(bytes) {
            if key != Key::Report {
                last = Some(key);
            }
        }
        let Some(last) = last else {
            return;
        };
        self.last_operator_input = Some(now);
        self.composer_empty = last == Key::Submit;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    /// Enter: `CR`, `LF`, or the kitty keyboard protocol's `CSI 13 … u` press.
    Submit,
    /// A report the terminal sends by itself: focus (`CSI I`/`CSI O`), SGR mouse (`CSI < … M/m`),
    /// a kitty key release (`CSI … : 3 u`).
    Report,
    Other,
}

/// Operator bytes as keys: one CSI sequence, or one byte.
struct Keys<'a>(&'a [u8]);

impl Iterator for Keys<'_> {
    type Item = Key;

    fn next(&mut self) -> Option<Key> {
        let (&first, rest) = self.0.split_first()?;
        if first == 0x1b && rest.first() == Some(&b'[') {
            let body = &rest[1..];
            if let Some(end) = body.iter().position(|b| (0x40..=0x7e).contains(b)) {
                self.0 = &body[end + 1..];
                return Some(csi_key(&body[..end], body[end]));
            }
            // A sequence cut off by the end of the write: nothing to read it as.
            self.0 = &[];
            return Some(Key::Other);
        }
        self.0 = rest;
        Some(match first {
            b'\r' | b'\n' => Key::Submit,
            _ => Key::Other,
        })
    }
}

fn csi_key(params: &[u8], fin: u8) -> Key {
    match fin {
        b'I' | b'O' if params.is_empty() => Key::Report,
        b'M' | b'm' if params.first() == Some(&b'<') => Key::Report,
        b'u' => {
            let mut fields = params.split(|&b| b == b';');
            let code = fields.next().unwrap_or_default();
            let code = code.split(|&b| b == b':').next().unwrap_or_default();
            let event = fields
                .next()
                .and_then(|mods| mods.split(|&b| b == b':').nth(1));
            if event == Some(b"3") {
                Key::Report
            } else if code == b"13" {
                Key::Submit
            } else {
                Key::Other
            }
        }
        _ => Key::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_harness::spec::DialogAnswer;

    fn scan_of(chunks: &[&str]) -> ModeScan {
        let mut scan = ModeScan::default();
        for c in chunks {
            scan.feed(c.as_bytes(), Instant::now());
        }
        scan
    }

    /// **Only text drawn under bracketed paste counts as a drawn screen**: escape sequences,
    /// control strings (a colour query's payload included), spaces and controls do not, and text
    /// from before the mode came on is forgotten when it does.
    #[test]
    fn a_screen_is_drawn_by_text_written_after_bracketed_paste_came_on() {
        let boot = [
            "\u{1b}[?1049h\u{1b}[?2004h\u{1b}[?25l\u{1b}[?12$p",
            "\u{1b}]10;?\u{1b}\\\u{1b}]11;?\u{7}\u{1b}P+q544e\u{1b}\\",
            "  \r\n\u{1b}[2J\u{1b}[H\u{1b}[38;2;1;2;3m",
        ];
        assert!(!scan_of(&boot).drawn(), "nothing visible yet");
        let mut drawn = boot.to_vec();
        drawn.push("> ready");
        assert!(scan_of(&drawn).drawn());
        assert!(
            scan_of(&["\u{1b}[?2004h\u{1b}]0;tit", "le\u{7}\u{2500}"]).drawn(),
            "UTF-8 text"
        );
        assert!(
            !scan_of(&[
                "\u{1b}[?2004h\u{1b}]0;a title not on the screen",
                " still\u{7}"
            ])
            .drawn(),
            "a control string's payload is not text, across reads"
        );
        assert!(
            !scan_of(&["boot banner", "\u{1b}[?2004h"]).drawn(),
            "text from before the mode is not a screen drawn under it"
        );
        assert!(
            !scan_of(&["\u{1b}[?2004h> ", "\u{1b}c"]).drawn(),
            "RIS resets it"
        );
        let mut scan = scan_of(&["\u{1b}[?2004h> "]);
        let before = scan.last_output().expect("output was seen");
        scan.feed(b"", Instant::now());
        assert_eq!(
            scan.last_output(),
            Some(before),
            "an empty read is not output"
        );
    }

    fn after(writes: &[&[u8]]) -> (bool, bool) {
        let mut typing = Typing::default();
        for w in writes {
            typing.operator_wrote(w, Instant::now());
        }
        (typing.last_operator_input.is_some(), typing.composer_empty)
    }

    #[test]
    fn the_last_key_decides_whether_the_composer_is_presumed_empty() {
        assert_eq!(after(&[]), (false, true));
        assert_eq!(after(&[b"abc"]), (true, false));
        assert_eq!(after(&[b"abc\r"]), (true, true));
        assert_eq!(after(&[b"abc\r", b"d"]), (true, false));
        assert_eq!(after(&[b"a\x1b[13u"]), (true, true), "kitty Enter");
        assert_eq!(
            after(&[b"a\x1b[13;1:1u"]),
            (true, true),
            "kitty Enter, event press"
        );
        assert_eq!(
            after(&[b"a\r\x1b[13;1:3u"]),
            (true, true),
            "its release is a report"
        );
        assert_eq!(
            after(&[b"\r\x1b[A"]),
            (true, false),
            "an arrow may recall history"
        );
        assert_eq!(
            after(&[b"\x1b[200~x\x1b[201~"]),
            (true, false),
            "an operator paste"
        );
        assert_eq!(
            after(&[b"a\x1b["]),
            (true, false),
            "a cut-off sequence is a key"
        );
    }

    #[test]
    fn reports_the_terminal_makes_by_itself_are_not_typing() {
        assert_eq!(after(&[b"\x1b[I\x1b[O"]), (false, true));
        assert_eq!(
            after(&[b"\x1b[<0;10;5M\x1b[<0;10;5m"]),
            (false, true),
            "SGR mouse"
        );
        assert_eq!(
            after(&[b"ab", b"\x1b[I"]),
            (true, false),
            "and do not submit"
        );
        assert_eq!(after(&[b"ab\r", b"\x1b[O"]), (true, true), "or dirty");
    }

    fn scanned(chunks: &[&[u8]]) -> bool {
        let mut scan = ModeScan::default();
        for c in chunks {
            scan.feed(c, Instant::now());
        }
        scan.bracketed_paste()
    }

    /// Every split point of a set, and of a reset after it, lands on the same answer.
    #[test]
    fn a_mode_change_is_seen_at_every_read_boundary() {
        let set = b"ab\x1b[?1004;2004hcd";
        for at in 0..=set.len() {
            assert!(scanned(&[&set[..at], &set[at..]]), "split at {at}");
        }
        let reset = b"\x1b[?2004l";
        for at in 0..=reset.len() {
            assert!(
                !scanned(&[set, &reset[..at], &reset[at..]]),
                "split at {at}"
            );
        }
    }

    #[test]
    fn sequences_that_are_not_decset_2004_leave_the_mode_alone() {
        assert!(!scanned(&[b"\x1b[2004h"]), "not private");
        assert!(!scanned(&[b"\x1b[?20045h"]), "another number");
        assert!(!scanned(&[b"\x1b[?2004$p"]), "a query");
        assert!(
            !scanned(&[b"\x1b[?20\x1b[m04h"]),
            "interrupted by another sequence"
        );
        assert!(!scanned(&[b"\x1b[?20\x1804h"]), "cancelled");
        assert!(scanned(&[b"\x1b]0;title\x07\x1b[?2004h"]), "after an OSC");
    }

    const HELD: BootDialog = BootDialog {
        needle: "No, exit Yes, I trust this folder",
        answer: DialogAnswer::Hold,
        note: "test",
    };

    fn watching(chunks: &[&[u8]]) -> ModeScan {
        let mut scan = ModeScan::default();
        scan.watch(std::slice::from_ref(&HELD));
        for c in chunks {
            scan.feed(c, Instant::now());
        }
        scan
    }

    /// **A dialog is recognised as drawn, not as sent**: claude writes the spaces between words
    /// as cursor moves (`No,` `ESC[8G` `exit`) and a line break between options, colours inside
    /// a word, and a read boundary can fall anywhere — all of it reads as the row's needle.
    #[test]
    fn a_needle_is_read_through_cursor_moves_colours_and_read_boundaries() {
        let drawn: &[u8] =
            b"\x1b[2G\x1b[38;2;1;2;3m\xe2\x9d\xaf\x1b[4GNo,\x1b[8Gexit\x1b[39m\r\r\n\x1b[4GYes,\
              \x1b[9GI\x1b[11Gtr\x1b[1mu\x1b[22mst\x1b[17Gthis\x1b[22Gfolder";
        for at in 0..=drawn.len() {
            assert_eq!(
                watching(&[&drawn[..at], &drawn[at..]]).showing(),
                Some(&HELD),
                "split at {at}"
            );
        }
        assert_eq!(watching(&[b"No, exit Yes, I trust"]).showing(), None);
        assert_eq!(
            watching(&[b"\x1b]0;No, exit Yes, I trust this folder\x07"]).showing(),
            None,
            "a control string's payload is not on the screen"
        );
    }

    /// **A key clears what the scan holds**: a dialog is on screen if it was drawn since anyone
    /// last typed, because a key is what dismisses one, and a dialog that survives it redraws.
    /// Marion's own keys also clear the drawn screen, so a boot waits for the TUI to redraw.
    #[test]
    fn a_key_forgets_the_screen_and_marions_key_also_forgets_the_drawing() {
        let dialog: &[u8] = b"\x1b[?2004hNo, exit\r\nYes, I trust this folder";
        let mut scan = watching(&[dialog]);
        scan.operator_typed();
        assert_eq!(scan.showing(), None);
        assert!(
            scan.drawn(),
            "an operator key leaves the drawn screen alone"
        );
        scan.feed(dialog, Instant::now());
        assert_eq!(scan.showing(), Some(&HELD), "a redraw shows it again");
        scan.marion_typed();
        assert_eq!(scan.showing(), None);
        assert!(!scan.drawn(), "marion's key waits for a redraw");
    }

    /// **The scan is bounded and ends with boot**: text past the cap drops its oldest half — a
    /// dialog seen before stays seen until a key — and once boot is over nothing is kept or
    /// matched at all.
    #[test]
    fn the_screen_text_is_bounded_and_dropped_once_boot_is_over() {
        let mut scan = watching(&[b"No, exit Yes, I trust this folder"]);
        let filler = vec![b'x'; SCREEN_TEXT_CAP];
        scan.feed(&filler, Instant::now());
        assert!(scan.screen_len() <= SCREEN_TEXT_CAP);
        assert_eq!(
            scan.showing(),
            Some(&HELD),
            "a dialog drawn once stays on screen while the TUI draws past the bound behind it"
        );
        scan.boot_over();
        scan.feed(b" No, exit Yes, I trust this folder", Instant::now());
        assert_eq!((scan.showing(), scan.screen_len()), (None, 0));
    }

    /// **Every row's needle is on the screen it was measured on** (`tests/fixtures/
    /// s37-boot-dialogs/`), and every measured screen shows some row's dialog — the row data and
    /// the recordings cannot drift apart silently.
    #[test]
    fn every_rows_boot_dialog_is_found_on_its_measured_first_screen() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/s37-boot-dialogs");
        let screens: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
            .expect("the fixture dir")
            .map(|e| e.unwrap().path())
            .map(|p| (p.display().to_string(), std::fs::read(&p).unwrap()))
            .collect();
        assert!(!screens.is_empty());
        let shows = |dialogs: &'static [BootDialog], bytes: &[u8]| {
            let mut scan = ModeScan::default();
            scan.watch(dialogs);
            scan.feed(bytes, Instant::now());
            scan.showing()
        };
        let mut matched = vec![false; screens.len()];
        for h in marion_core::harness::Harness::ALL {
            let dialogs = marion_harness::adapter::harness_spec(h)
                .boot_dialogs
                .dialogs;
            for d in dialogs {
                let found = screens
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, bytes))| shows(std::slice::from_ref(d), bytes).is_some());
                let mut any = false;
                for (i, _) in found {
                    matched[i] = true;
                    any = true;
                }
                assert!(any, "{h}: {:?} is on no measured screen", d.needle);
            }
            // The row's own order decides: on its screen, the first needle it lists wins.
            if let Some(first) = dialogs.first() {
                assert!(
                    screens
                        .iter()
                        .any(|(_, b)| shows(dialogs, b) == Some(first)),
                    "{h}: its first dialog never wins on a measured screen"
                );
            }
        }
        for ((name, _), m) in screens.iter().zip(matched) {
            assert!(m, "{name} shows no row's dialog");
        }
    }

    /// A parameter list past the bound is dropped whole rather than read truncated.
    #[test]
    fn an_overlong_parameter_list_is_not_read() {
        let mut long = b"\x1b[?".to_vec();
        long.extend(std::iter::repeat_n(b'1', MAX_PARAMS));
        long.extend(b";2004h");
        assert!(!scanned(&[&long]));
        assert!(
            scanned(&[&long, b"\x1b[?2004h"]),
            "and the scanner recovers"
        );
    }
}
