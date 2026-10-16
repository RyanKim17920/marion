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
//! And whether it has **set its window title** since: codex 0.155.1 draws a provisional composer
//! that drops Enter while it boots, and only its real one sets the title.
//!
//! And the screen as it stands ([`Grid`]), for the row's mid-session asks: an approval prompt
//! is up while its footer is on it, or its header with one of its option lines.
//!
//! The other is read off the operator's own writes ([`Typing`]): when a person last typed into
//! this terminal, and whether what they typed has been submitted. A paste landing in the middle of
//! a half-typed line would be submitted *with* it — the operator's words and marion's as one turn.

use std::time::{Duration, Instant};

use marion_harness::spec::{Ask, BootDialog, BootSignal};

/// Longest parameter list kept for one private-mode CSI. Real mode sets are a handful of short
/// numbers; anything longer is not one marion needs to read, and a bound keeps a hostile stream
/// from growing this.
const MAX_PARAMS: usize = 64;

/// The DEC private mode for bracketed paste.
const BRACKETED_PASTE: &[u8] = b"2004";

/// Longest OSC command number kept: every command marion reads is one digit.
const MAX_OSC_DIGITS: usize = 4;

/// **The node's screen as it stands now**, cell for cell, for the row's [`Ask`]s: what a prompt's
/// lines are looked for on. A terminal emulator's grid rather than the text written,
/// because a reply can quote a prompt's line and scroll it away, and a TUI that redraws only the
/// cells that changed (claude) leaves a prompt's header standing while it moves the selection.
struct Grid {
    parser: vt100::Parser,
    /// The screen's text with every run of whitespace one space, reused across looks.
    words: String,
    /// The rows that reach the right edge, as [`Self::words`] spells them: where a TUI cuts a
    /// line too long for the pane.
    edges: Vec<String>,
    /// How many times the screen was read whole ([`Self::showing`]).
    looks: u64,
}

/// The shortest start of a needle that, cut off at the pane's right edge, still reads as it: a
/// pane narrower than a prompt's line shows the line's first words and nothing of the rest.
const MIN_CUT: usize = 16;

impl std::fmt::Debug for Grid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grid")
            .field("size", &self.parser.screen().size())
            .finish_non_exhaustive()
    }
}

impl Grid {
    fn new(rows: u16, cols: u16) -> Grid {
        // No scrollback: only what is on the screen now counts.
        Grid {
            parser: vt100::Parser::new(rows.max(1), cols.max(1), 0),
            words: String::new(),
            edges: Vec::new(),
            looks: 0,
        }
    }

    /// **The first of `asks` on the screen**: its footer, or its header together with its option
    /// line, each whole or cut off at the right edge. The footer alone is enough, because a
    /// prompt taller than the pane pushes its header off the top; the header alone is not,
    /// because an agent's idle reply can end on the same sentence, and that reply left standing
    /// would hold the steer that answers it for good. A narrow pane cuts any of the lines.
    fn showing(&mut self, asks: &'static [Ask]) -> Option<&'static Ask> {
        self.looks += 1;
        let screen = self.parser.screen();
        let cols = usize::from(screen.size().1);
        self.words.clear();
        self.edges.clear();
        let mut joined = true;
        for (i, row) in screen.rows(0, screen.size().1).enumerate() {
            // A row the terminal wrapped runs on into the next with no space between.
            if !joined && !self.words.is_empty() {
                self.words.push(' ');
            }
            let mut first = true;
            for word in row.split_whitespace() {
                if !first {
                    self.words.push(' ');
                }
                first = false;
                self.words.push_str(word);
            }
            if row.chars().count() + 1 >= cols || row.trim_end().ends_with('…') {
                let edge: Vec<&str> = row.split_whitespace().collect();
                self.edges
                    .push(edge.join(" ").trim_end_matches('…').trim_end().to_string());
            }
            joined = u16::try_from(i).is_ok_and(|i| screen.row_wrapped(i));
        }
        let on = |needle: &str| {
            self.words.contains(needle) || self.edges.iter().any(|row| cut_at_edge(row, needle))
        };
        asks.iter()
            .find(|a| on(a.footer) || (on(a.header) && on(a.option)))
    }
}

/// `row` is the start of `needle`, at least [`MIN_CUT`] characters of it: the needle, cut off by
/// the pane's edge. The row is all of it — prose that only ends on a needle's first words, as a
/// soft-wrapped reply can at the edge, is not the prompt's line.
fn cut_at_edge(row: &str, needle: &str) -> bool {
    row.chars().count() >= MIN_CUT && needle.starts_with(row)
}

/// **When a node's boot is over without a paste**: the row's boot mark under bracketed paste, no
/// boot dialog on the screen, then `settle` of quiet — the moment a first paste would be typed
/// (`crate::paste::gate`). Past it the boot dialogs are not looked for: a heading drawn later is
/// the session's text, not a dialog in front of the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootMark {
    pub mark: BootSignal,
    pub settle: Duration,
}

/// How much visible text [`ModeScan::ask_tail`] holds before it is searched and cut: past twice
/// the longest ask word, so a word cut by a read boundary is still whole in it.
const ASK_TAIL: usize = 128;

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
    /// After `ESC ]`, collecting the OSC command number up to its `;`.
    OscCommand,
    /// Inside a control string (OSC, DCS, APC, PM, SOS): its payload is not text on the screen.
    /// Ended by `BEL`, or by `ESC \` through [`Scan::Escape`].
    Str,
}

/// **DECSET/DECRST 2004, followed across reads, and whether text has been drawn or a window title
/// set since.** A small state machine over the output stream — only the sequences that change the
/// mode or set a title are recognised, every other control string is skipped whole, and a read
/// boundary anywhere inside one is carried, because a `read()` is not a frame (S11).
#[derive(Debug, Default)]
pub(crate) struct ModeScan {
    /// The screen, for [`Self::asks`]: kept from the first byte, because a prompt can be up before
    /// anyone watches for it, and dropped for good by a [`Self::watch`] with no asks. `None` from
    /// [`Default`] (a scan with no screen size, in tests of the stream alone).
    grid: Option<Grid>,
    state: Scan,
    params: Vec<u8>,
    overflowed: bool,
    bracketed_paste: bool,
    drawn: bool,
    titled: bool,
    last_output: Option<Instant>,
    /// The row's boot dialogs this scan recognises ([`Self::showing`]).
    dialogs: &'static [BootDialog],
    /// The row's mid-session asks this scan recognises on [`Self::grid`], before boot and after
    /// it ([`Self::asking`]).
    asks: &'static [Ask],
    /// The ask on the grid as of the last read.
    ask: Option<&'static Ask>,
    /// The words of [`Self::asks`]' lines, four letters or longer: a prompt cannot
    /// come onto the screen without one of them written in the stream, so the grid is read whole
    /// only after one is (or while an ask is up, to see it leave).
    ask_words: Vec<&'static str>,
    /// The last of the visible text written, words joined by one space, searched for
    /// [`Self::ask_words`]; bounded by [`ASK_TAIL`].
    ask_tail: Vec<u8>,
    /// How much of [`Self::ask_tail`] was searched: a word counts once, when it is written.
    ask_searched: usize,
    /// The grid may show an ask it did not at the last read: an ask word was written, the screen
    /// switched or was reset.
    ask_hint: bool,
    /// The visible text written since anyone last typed, words joined by one space — what a
    /// dialog's needle is matched against. Kept from the first byte, because a TUI can draw its
    /// dialog before a paste driver is attached, and dropped for good at [`Self::boot_over`].
    screen: Vec<u8>,
    /// How much of [`Self::screen`] every watched needle has been looked for in: a look reads only
    /// what came after it (and a needle's length before), so a TUI that keeps drawing costs each
    /// byte one look, not one per read.
    scanned: usize,
    /// A space, a line break or a cursor move came after the last visible byte.
    gap: bool,
    /// Bit `i`: `dialogs[i]` was drawn since anyone last typed. Latched rather than re-read from
    /// [`Self::screen`], because a TUI that animates behind its dialog (codex's welcome art)
    /// writes past the cap while the dialog, drawn once, stays on screen.
    seen: u64,
    /// What [`Self::showing`] or [`Self::asking`] answers changed since [`Self::take_changed`].
    changed: bool,
    /// Visible text arrived since the last look for needles.
    fresh: bool,
    /// What ends boot where no paste does ([`Self::settle_boot`]); `None` for a row with none.
    boot: Option<BootMark>,
    boot_over: bool,
}

impl ModeScan {
    /// A scan of a terminal `rows` by `cols`, keeping its screen for the row's asks.
    pub(crate) fn new(rows: u16, cols: u16) -> ModeScan {
        ModeScan {
            grid: Some(Grid::new(rows, cols)),
            ..ModeScan::default()
        }
    }

    /// The terminal is now `rows` by `cols`. The node redraws for it; the grid is resized first,
    /// as the terminal it stands for was.
    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        if let Some(grid) = &mut self.grid {
            grid.parser.screen_mut().set_size(rows.max(1), cols.max(1));
            self.look_for_asks();
        }
    }

    /// Whether the node has asked for bracketed paste and not since withdrawn it.
    pub(crate) fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    /// Whether the node has written visible text — a byte that is neither a control, a space nor
    /// part of an escape sequence or control string — since it last turned bracketed paste on.
    pub(crate) fn drawn(&self) -> bool {
        self.drawn
    }

    /// Whether the node has set its window title (OSC 0 or OSC 2, any payload) since it last
    /// turned bracketed paste on. Not cleared by typing: a title belongs to the TUI's real
    /// screen, and a dialog answered after it is covered by [`Self::drawn`]'s reset.
    pub(crate) fn titled(&self) -> bool {
        self.titled
    }

    /// When the node last wrote anything at all, `None` before its first byte.
    pub(crate) fn last_output(&self) -> Option<Instant> {
        self.last_output
    }

    /// Recognise `dialogs` before the first paste and `asks` at any time, on the screen from now
    /// on — over what was drawn already, too.
    pub(crate) fn watch(
        &mut self,
        dialogs: &'static [BootDialog],
        asks: &'static [Ask],
        boot: Option<BootMark>,
    ) {
        // One bit per dialog; a row lists two or three.
        if !self.boot_over {
            self.dialogs = &dialogs[..dialogs.len().min(64)];
            self.boot = boot;
        }
        self.asks = asks;
        self.ask_words = asks
            .iter()
            .flat_map(|a| [a.header, a.footer, a.option])
            .flat_map(|line| line.split(' '))
            .filter(|w| w.chars().count() >= 4)
            .collect();
        self.ask_words.sort_unstable();
        self.ask_words.dedup();
        if asks.is_empty() {
            self.grid = None;
            self.ask_tail = Vec::new();
        }
        self.forget_latches();
        self.scanned = 0;
        self.look();
        self.look_for_asks();
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

    /// **The first of the watched asks on the screen now**, in the row's order: its footer stands
    /// on the grid, or its header with its option line ([`Grid::showing`]). Before boot and after it, and no key clears it — only the
    /// prompt leaving the screen does.
    pub(crate) fn asking(&self) -> Option<&'static Ask> {
        self.ask
    }

    /// Whether an ask word was written since the last search, and the tail cut back to what a
    /// word cut by the next read could need.
    fn search_ask_tail(&mut self) {
        let (tail, searched) = (&self.ask_tail, self.ask_searched);
        self.ask_hint |= self.ask_words.iter().any(|w| {
            tail.windows(w.len())
                .enumerate()
                .any(|(at, t)| at + w.len() > searched && t == w.as_bytes())
        });
        let keep = ASK_TAIL / 4;
        if self.ask_tail.len() > keep {
            self.ask_tail.drain(..self.ask_tail.len() - keep);
        }
        self.ask_searched = self.ask_tail.len();
    }

    /// Read the grid for the watched asks.
    fn look_for_asks(&mut self) {
        let Some(grid) = &mut self.grid else {
            return;
        };
        let ask = grid.showing(self.asks);
        self.changed |= ask != self.ask;
        self.ask = ask;
    }

    /// The row's boot mark is on the screen.
    fn marked(&self, mark: BootSignal) -> bool {
        match mark {
            BootSignal::FirstDraw => self.drawn,
            BootSignal::WindowTitle => self.titled && self.drawn,
        }
    }

    /// **When boot will be over, as the screen stands**: the last output plus the row's settle,
    /// once its mark is up and no boot dialog is showing. `None` while either is missing, and
    /// once boot is over.
    pub(crate) fn boot_due(&self) -> Option<Instant> {
        let boot = self.boot.filter(|_| !self.boot_over)?;
        if !self.marked(boot.mark) || self.showing().is_some() {
            return None;
        }
        Some(self.last_output? + boot.settle)
    }

    /// End boot if it is due by `now` ([`Self::boot_due`]); otherwise when it will be.
    pub(crate) fn settle_boot(&mut self, now: Instant) -> Option<Instant> {
        let due = self.boot_due()?;
        if now < due {
            return Some(due);
        }
        self.boot_over();
        None
    }

    /// Whether [`Self::showing`] or [`Self::asking`] changed since the last call.
    pub(crate) fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    /// Latch every watched dialog's needle the text kept since the last look now holds.
    fn look(&mut self) {
        self.fresh = false;
        let longest = self
            .dialogs
            .iter()
            .map(|d| d.needle.len())
            .max()
            .unwrap_or(0);
        // A needle that ends in the new text may start up to its length before it.
        let from = self.scanned.saturating_sub(longest.saturating_sub(1));
        let hay = &self.screen[from.min(self.screen.len())..];
        let before = self.seen;
        for (i, d) in self.dialogs.iter().enumerate() {
            let needle = d.needle.as_bytes();
            if self.seen & (1 << i) == 0
                && !needle.is_empty()
                && hay.windows(needle.len()).any(|w| w == needle)
            {
                self.seen |= 1 << i;
            }
        }
        self.changed |= self.seen != before;
        self.scanned = self.screen.len();
    }

    /// No boot dialog is on the screen any more as far as the latches know.
    fn forget_latches(&mut self) {
        self.changed |= self.seen != 0;
        self.seen = 0;
    }

    /// The operator typed: a key is what dismisses a boot dialog, and one that survives it
    /// redraws. An ask is read off the grid instead: an arrow moves its selection, a focus report
    /// is no key at all, and either leaves the prompt up.
    pub(crate) fn operator_typed(&mut self) {
        self.screen.clear();
        self.scanned = 0;
        self.gap = false;
        self.forget_latches();
    }

    /// Marion typed: as [`Self::operator_typed`], and the drawn screen is forgotten too, so a
    /// first paste after marion answered a dialog waits for the TUI to redraw.
    pub(crate) fn marion_typed(&mut self) {
        self.operator_typed();
        self.drawn = false;
    }

    /// The first paste landed: no boot dialog is looked for again, and no screen text is kept for
    /// them. The asks' grid stays.
    pub(crate) fn boot_over(&mut self) {
        self.boot_over = true;
        self.dialogs = &[];
        self.forget_latches();
        self.screen = Vec::new();
        self.scanned = 0;
    }

    #[cfg(test)]
    pub(crate) fn screen_len(&self) -> usize {
        self.screen.len()
    }

    fn visible(&mut self, b: u8) {
        if !self.ask_words.is_empty() {
            if self.ask_tail.len() >= ASK_TAIL {
                self.search_ask_tail();
            }
            if self.gap && !self.ask_tail.is_empty() {
                self.ask_tail.push(b' ');
            }
            self.ask_tail.push(b);
        }
        if self.boot_over {
            // The gap is the screen text's to clear, and none is kept past boot.
            self.gap = false;
            return;
        }
        if self.screen.len() >= SCREEN_TEXT_CAP {
            // Every needle is far shorter than the half kept, so one still being drawn survives
            // the drain whole, and one already drawn is latched before it goes.
            self.look();
            self.screen.drain(..SCREEN_TEXT_CAP / 2);
            self.scanned = self.scanned.saturating_sub(SCREEN_TEXT_CAP / 2);
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
        let due = self.boot_due().is_some();
        for &b in chunk {
            self.step(b);
        }
        // Boot coming due is news to whoever waits for it ([`Self::take_changed`]).
        self.changed |= !due && self.boot_due().is_some();
        if self.fresh && !self.dialogs.is_empty() {
            self.look();
        }
        if let Some(grid) = &mut self.grid {
            grid.parser.process(chunk);
            if !self.asks.is_empty() {
                self.search_ask_tail();
                if std::mem::take(&mut self.ask_hint)
                    || self.ask.is_some()
                    || self.ask_words.is_empty()
                {
                    self.look_for_asks();
                }
            }
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
                b']' => {
                    self.params.clear();
                    Scan::OscCommand
                }
                b'P' | b'_' | b'^' | b'X' => Scan::Str,
                // RIS: a full reset, which clears every DEC private mode.
                b'c' => {
                    self.ask_hint = true;
                    self.bracketed_paste = false;
                    self.drawn = false;
                    self.titled = false;
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
                    // A screen switched back (the alternate screen's end) shows what was on it.
                    self.ask_hint = true;
                    if self
                        .params
                        .split(|&c| c == b';')
                        .any(|p| p == BRACKETED_PASTE)
                    {
                        let on = b == b'h';
                        if on && !self.bracketed_paste {
                            // What counts is a screen drawn under this mode, not before it.
                            self.drawn = false;
                            self.titled = false;
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
            Scan::OscCommand => match b {
                b'0'..=b'9' if self.params.len() < MAX_OSC_DIGITS => {
                    self.params.push(b);
                    Scan::OscCommand
                }
                b';' => {
                    // 0 sets the icon name and title, 2 the title alone.
                    if matches!(self.params.as_slice(), b"0" | b"2") {
                        self.titled |= self.bracketed_paste;
                    }
                    Scan::Str
                }
                0x07 => Scan::Ground,
                _ => Scan::Str,
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
    /// The node has set its window title since it turned bracketed paste on
    /// ([`ModeScan::titled`]).
    pub titled: bool,
    /// When the node last wrote anything — `None` before its first byte.
    pub last_output: Option<Instant>,
    /// The row's boot dialog drawn since anyone last typed, before the first paste
    /// ([`ModeScan::showing`]).
    pub boot_dialog: Option<&'static BootDialog>,
    /// The row's mid-session ask on the screen now ([`ModeScan::asking`]).
    pub ask: Option<&'static Ask>,
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
        action: "trust {repo} once",
        answer: DialogAnswer::Hold,
        note: "test",
    };

    fn watching(chunks: &[&[u8]]) -> ModeScan {
        let mut scan = ModeScan::default();
        scan.watch(std::slice::from_ref(&HELD), &[], None);
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
            scan.watch(dialogs, &[], None);
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

    const ASK: Ask = Ask {
        header: "Would you like to run the following command?",
        footer: "Press enter to confirm or esc to cancel",
        option: "1. Yes, proceed (y)",
        action: "answer the approval",
        note: "test",
    };

    /// A scan of a 24 by 80 terminal watching [`ASK`], booted.
    fn asked() -> ModeScan {
        let mut scan = ModeScan::new(24, 80);
        scan.watch(
            std::slice::from_ref(&HELD),
            std::slice::from_ref(&ASK),
            None,
        );
        scan.boot_over();
        scan.feed(b"\x1b[?2004h\x1b[2J\x1b[H> composer", Instant::now());
        scan
    }

    /// The approval prompt, drawn over the composer: header, an option, footer.
    const PROMPT: &[u8] = b"\x1b[2;3HWould\x1b[2;9Hyou like to run the following command?\
        \x1b[4;3H\xe2\x80\xba 1. Yes, proceed (y)\x1b[5;5H2. No\
        \x1b[7;3HPress enter to confirm or esc to cancel";

    /// **An ask is up while its header and its footer are on the screen**, at every read
    /// boundary, after boot — and no key takes it down, only the prompt leaving the screen.
    #[test]
    fn an_ask_is_up_while_header_and_footer_are_on_the_screen_until_it_leaves() {
        for at in 0..=PROMPT.len() {
            let mut scan = asked();
            assert_eq!(scan.asking(), None);
            scan.feed(&PROMPT[..at], Instant::now());
            scan.feed(&PROMPT[at..], Instant::now());
            assert_eq!(scan.asking(), Some(&ASK), "split at {at}");
            assert!(scan.take_changed() && !scan.take_changed());
            assert_eq!(scan.showing(), None, "boot dialogs end with boot");
            scan.operator_typed();
            assert_eq!(scan.asking(), Some(&ASK), "a key alone takes nothing down");
            scan.feed(b"\x1b[2J\x1b[H> composer", Instant::now());
            assert_eq!(scan.asking(), None, "the prompt left the screen");
            assert!(scan.take_changed());
        }
    }

    /// **The footer holds alone, the header only with an option line**: the footer of a prompt
    /// taller than the pane (its header scrolled off the top) holds, and so does the header over
    /// an option with the footer below the pane. An idle reply that ends on the header, or quotes
    /// it, holds nothing — a hold on it would keep the steer that answers it waiting for good —
    /// and neither does an option line alone.
    #[test]
    fn the_footer_or_the_header_with_an_option_is_an_ask_and_the_header_alone_is_not() {
        let mut scan = asked();
        scan.feed(
            b"\x1b[24;1H  Would you like to run the following command?",
            Instant::now(),
        );
        assert_eq!(scan.asking(), None, "the header alone");
        scan.feed(
            b"\x1b[22;1H  Would you like to run the following command?\
              \x1b[23;1H  \xe2\x80\xba 1. Yes, proceed (y)",
            Instant::now(),
        );
        assert_eq!(scan.asking(), Some(&ASK), "the header over an option");

        let mut scan = asked();
        scan.feed(b"\x1b[5;3H\xe2\x80\xba 1. Yes, proceed (y)", Instant::now());
        assert_eq!(scan.asking(), None, "an option alone");

        let mut scan = asked();
        scan.feed(
            b"\x1b[2;1H  Would you like to run the following command?",
            Instant::now(),
        );
        // A command longer than the pane: the header scrolls off before the footer is drawn.
        scan.feed(&b"\r\n  $ printf line".repeat(30), Instant::now());
        scan.feed(
            b"\r\n  Press enter to confirm or esc to cancel",
            Instant::now(),
        );
        assert_eq!(scan.asking(), Some(&ASK), "the footer of a tall prompt");

        let mut scan = asked();
        scan.feed(
            b"\r\n  I will ask: Would you like to run the following command?\r\n> ",
            Instant::now(),
        );
        assert_eq!(scan.asking(), None, "an idle reply that quotes the header");
        scan.feed(
            b"\r\n  It says: Press enter to confirm or esc to cancel\r\n",
            Instant::now(),
        );
        assert_eq!(
            scan.asking(),
            Some(&ASK),
            "a quoted footer holds while it shows"
        );
        scan.feed(&b"\r\n more of the reply".repeat(30), Instant::now());
        assert_eq!(scan.asking(), None, "and lets go once it scrolls away");
    }

    /// **A pane narrower than a prompt's lines cuts them at its edge, and the cut line still
    /// reads as the line**: the header's first words up to the right edge (over an option), the
    /// footer's likewise, with or without the TUI's `…`. The same words short of the edge are not
    /// a cut line, and neither is a row of prose that only ends on a line's first words.
    #[test]
    fn a_line_cut_at_a_narrow_panes_edge_is_an_ask() {
        let narrow = || {
            let mut scan = ModeScan::new(24, 30);
            scan.watch(&[], std::slice::from_ref(&ASK), None);
            scan.feed(b"\x1b[?2004h\x1b[2J\x1b[H> composer", Instant::now());
            scan
        };
        let mut scan = narrow();
        // 30 columns: `  Would you like to run the fo` exactly.
        scan.feed(b"\x1b[3;1H  Would you like to run the fo", Instant::now());
        assert_eq!(scan.asking(), None, "the header cut at the edge, alone");
        scan.feed(b"\x1b[5;3H1. Yes, proceed (y)", Instant::now());
        assert_eq!(scan.asking(), Some(&ASK), "the cut header over an option");
        let mut scan = narrow();
        scan.feed(
            b"\x1b[9;1H  Press enter to confirm or e\xe2\x80\xa6",
            Instant::now(),
        );
        assert_eq!(
            scan.asking(),
            Some(&ASK),
            "the footer cut, with an ellipsis"
        );
        let mut scan = narrow();
        // 30 columns of prose ending on the footer's first 19 characters, then the option.
        scan.feed(
            b"\x1b[3;1Hso it read Press enter to conf\x1b[5;3H1. Yes, proceed (y)",
            Instant::now(),
        );
        assert_eq!(
            scan.asking(),
            None,
            "a row that only ends on a line's start is prose"
        );
        let mut scan = narrow();
        scan.feed(b"\x1b[3;1H  Would you like to run", Instant::now());
        assert_eq!(
            scan.asking(),
            None,
            "short of the edge, a phrase is a phrase"
        );
    }

    /// **The grid is read whole only when it may show something new**: an ask word written, or
    /// an ask up (to see it leave). A model streaming a reply that names none costs no read of
    /// the screen per chunk — one per chunk before (a thousand here), none now.
    #[test]
    fn the_screen_is_read_only_after_an_ask_word_or_while_an_ask_is_up() {
        let mut scan = asked();
        let looks = |s: &ModeScan| s.grid.as_ref().map_or(0, |g| g.looks);
        let before = looks(&scan);
        for i in 0..1000 {
            scan.feed(
                format!(
                    "\x1b[{};3Hthe model streams its answer, token {i}",
                    i % 20 + 1
                )
                .as_bytes(),
                Instant::now(),
            );
        }
        assert_eq!(looks(&scan) - before, 0, "no ask word, no read");
        // An ask word cut by a read boundary is still seen, once.
        scan.feed(b"\x1b[5;3HWould you like to run the follo", Instant::now());
        scan.feed(b"wing command?", Instant::now());
        assert_eq!(scan.asking(), None, "the header alone is no ask");
        scan.feed(b"\x1b[6;3H1. Yes, proceed (y)", Instant::now());
        assert_eq!(scan.asking(), Some(&ASK));
        let up = looks(&scan);
        scan.feed(b"\x1b[7;3Hstill drawing", Instant::now());
        assert_eq!(
            looks(&scan),
            up + 1,
            "while an ask is up, every chunk is read"
        );
        scan.feed(b"\x1b[2J\x1b[H> composer", Instant::now());
        assert_eq!(scan.asking(), None);
        let gone = looks(&scan);
        scan.feed(b"\x1b[3;3Hthe next answer", Instant::now());
        assert_eq!(looks(&scan), gone, "and none once it left");
    }

    /// **A prompt redrawn by its changed cells alone stays up**: the selection moves under an
    /// arrow key and only the option rows are written; a second prompt drawn over the first
    /// keeps the header cells it shares. Overwriting the footer and the header, or the footer and
    /// the options, takes it down.
    #[test]
    fn a_prompt_redrawn_without_its_header_stays_up() {
        let mut scan = asked();
        scan.feed(PROMPT, Instant::now());
        scan.operator_typed();
        scan.feed(
            b"\x1b[4;3H  1. Yes, proceed (y)\x1b[5;3H\xe2\x80\xba 2. No",
            Instant::now(),
        );
        assert_eq!(scan.asking(), Some(&ASK), "the selection moved");
        scan.feed(b"\x1b[I\x1b[O", Instant::now());
        assert_eq!(scan.asking(), Some(&ASK));
        scan.feed(b"\x1b[3;3H$ cargo test\x1b[3;15H\x1b[K", Instant::now());
        assert_eq!(
            scan.asking(),
            Some(&ASK),
            "a second command, header unchanged"
        );
        scan.feed(b"\x1b[2;1H\x1b[2K> composer", Instant::now());
        assert_eq!(
            scan.asking(),
            Some(&ASK),
            "the header overwritten, the footer stands"
        );
        scan.feed(b"\x1b[7;1H\x1b[2K", Instant::now());
        assert_eq!(scan.asking(), None, "header and footer gone");

        let mut scan = asked();
        scan.feed(PROMPT, Instant::now());
        scan.feed(b"\x1b[7;1H\x1b[2K", Instant::now());
        assert_eq!(
            scan.asking(),
            Some(&ASK),
            "the footer overwritten, header and option stand"
        );
        scan.feed(b"\x1b[4;1H\x1b[2K\x1b[5;1H\x1b[2K", Instant::now());
        assert_eq!(scan.asking(), None, "the options gone too");
    }

    /// **A resize keeps the grid in step**: a prompt drawn at the new size is read at it.
    #[test]
    fn an_ask_is_read_at_the_terminals_size() {
        let mut scan = asked();
        scan.resize(40, 120);
        scan.feed(
            b"\x1b[30;3HWould you like to run the following command?\
              \x1b[35;70HPress enter to confirm or esc to cancel",
            Instant::now(),
        );
        assert_eq!(scan.asking(), Some(&ASK));
        let mut none = ModeScan::new(24, 80);
        none.watch(&[], &[], None);
        none.feed(PROMPT, Instant::now());
        assert_eq!(none.asking(), None, "a row with no asks keeps no grid");
        assert!(none.grid.is_none());
    }

    /// **Every row's ask is on a screen it was measured on** (`tests/fixtures/pane-asks/`, at
    /// conformance's 120 by 40), by its footer and by its header with its option, and every
    /// recorded ask screen shows one of some row's asks.
    #[test]
    fn every_rows_ask_is_found_on_its_measured_screen() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/pane-asks");
        let screens: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
            .expect("the fixture dir")
            .map(|e| e.unwrap().path())
            .map(|p| (p.display().to_string(), std::fs::read(&p).unwrap()))
            .collect();
        assert!(!screens.is_empty());
        let asks_on = |asks: &'static [Ask], bytes: &[u8]| {
            let mut scan = ModeScan::new(40, 120);
            scan.watch(&[], asks, None);
            // In reads of a pty's size, as the host sees them.
            for chunk in bytes.chunks(1024) {
                scan.feed(chunk, Instant::now());
            }
            scan.asking()
        };
        let mut matched = vec![false; screens.len()];
        for h in marion_core::harness::Harness::ALL {
            for a in marion_harness::adapter::harness_spec(h).boot_dialogs.asks {
                let mut any = false;
                for (i, (_, bytes)) in screens.iter().enumerate() {
                    if asks_on(std::slice::from_ref(a), bytes).is_some() {
                        matched[i] = true;
                        any = true;
                    }
                }
                assert!(any, "{h}: ask {:?} is on no measured screen", a.header);
                // Its header and option, too, are on its screen, as drawn: the option is the
                // prompt's own spelling, not a guess at it.
                let headed: &'static [Ask] = Box::leak(Box::new([Ask {
                    footer: "a footer no screen shows",
                    ..*a
                }]));
                assert!(
                    screens
                        .iter()
                        .any(|(_, bytes)| asks_on(headed, bytes).is_some()),
                    "{h}: ask {:?} with option {:?} is on no measured screen",
                    a.header,
                    a.option
                );
            }
        }
        for ((name, _), m) in screens.iter().zip(matched) {
            assert!(m, "{name} shows no row's ask");
        }
    }

    /// **Boot ends on the row's mark and settle, with no paste**, and a folder dialog drawn
    /// after it is the session's text. Before it, codex's folder heading alone is no dialog
    /// either: the row holds on the last option and footer.
    #[test]
    fn boot_ends_on_the_rows_mark_and_settle_and_a_later_dialog_is_session_text() {
        let codex = marion_harness::adapter::harness_spec(marion_core::harness::Harness::Codex)
            .boot_dialogs;
        let settle = Duration::from_millis(1500);
        let mark = BootMark {
            mark: BootSignal::WindowTitle,
            settle,
        };
        let folder: &[u8] =
            include_bytes!("../../../../tests/fixtures/s37-boot-dialogs/codex-0.160.1.raw");
        let t0 = Instant::now();

        let mut heading = ModeScan::new(40, 120);
        heading.watch(codex.dialogs, codex.asks, Some(mark));
        heading.feed(
            b"\x1b[?2004h\x1b]0;repo\x07  Folder access is restricted here\r\n",
            t0,
        );
        assert_eq!(heading.showing(), None, "the heading alone");

        let mut scan = ModeScan::new(40, 120);
        scan.watch(codex.dialogs, codex.asks, Some(mark));
        scan.feed(folder, t0);
        assert!(scan.showing().is_some(), "the measured folder screen");
        assert_eq!(scan.boot_due(), None, "not while a dialog shows");
        scan.operator_typed();
        let _ = scan.take_changed();
        scan.feed(b"\x1b]0;repo\x07\x1b[2J\x1b[H> composer", t0);
        assert!(scan.take_changed(), "boot coming due is news");
        assert_eq!(scan.boot_due(), Some(t0 + settle));
        assert_eq!(
            scan.settle_boot(t0 + settle - Duration::from_millis(1)),
            Some(t0 + settle)
        );
        assert_eq!(scan.settle_boot(t0 + settle), None);
        assert_eq!(
            (scan.boot_due(), scan.screen_len()),
            (None, 0),
            "boot is over"
        );
        scan.feed(folder, t0 + settle * 2);
        assert_eq!(
            scan.showing(),
            None,
            "a folder screen after boot is not a dialog"
        );

        let mut unmarked = ModeScan::new(40, 120);
        unmarked.watch(codex.dialogs, codex.asks, None);
        unmarked.feed(b"\x1b[?2004h\x1b]0;repo\x07> composer", t0);
        assert_eq!(unmarked.settle_boot(t0 + settle * 10), None);
        unmarked.feed(folder, t0 + settle * 10);
        assert!(
            unmarked.showing().is_some(),
            "a row with no mark watches until its first paste"
        );
    }

    fn titled(chunks: &[&[u8]]) -> bool {
        let mut scan = ModeScan::default();
        for c in chunks {
            scan.feed(c, Instant::now());
        }
        scan.titled()
    }

    /// **A window title set under bracketed paste is seen**, OSC 0 or 2, ended by `BEL` or `ST`,
    /// at every read boundary — and nothing else is a title.
    #[test]
    fn a_window_title_under_bracketed_paste_is_seen_at_every_read_boundary() {
        for set in [
            &b"\x1b[?2004h> draft\x1b]0;repo\x07"[..],
            b"\x1b[?2004h\x1b]2;repo\x1b\\",
        ] {
            for at in 0..=set.len() {
                assert!(titled(&[&set[..at], &set[at..]]), "{set:?} split at {at}");
            }
        }
        assert!(
            titled(&[b"\x1b[?2004h\x1b]0;\x07"]),
            "an empty title is still a title"
        );
        assert!(
            !titled(&[b"\x1b[?2004h> draft"]),
            "text alone is not a title"
        );
        assert!(
            !titled(&[b"\x1b]0;early\x07\x1b[?2004h> draft"]),
            "a title before bracketed paste is not one set under it"
        );
        assert!(
            !titled(&[b"\x1b[?2004h\x1b]10;?\x07\x1b]11;?\x1b\\\x1b]8;;x\x07"]),
            "colour queries and hyperlinks are not titles"
        );
        assert!(
            !titled(&[b"\x1b[?2004h\x1b]00000;t\x07"]),
            "an overlong number"
        );
        assert!(!titled(&[b"\x1b[?2004h\x1b]0\x07"]), "ended before its `;`");
        assert!(
            !titled(&[b"\x1b[?2004h\x1b]0;t\x07\x1b[?2004l\x1b[?2004h"]),
            "bracketed paste turned on again starts over"
        );
        assert!(
            !titled(&[b"\x1b[?2004h\x1b]0;t\x07\x1bc\x1b[?2004h"]),
            "RIS resets it"
        );
        assert!(
            scanned(&[b"\x1b[?2004h\x1b]0;t\x07\x1b[?2004l\x1b[?2004h"]),
            "a title does not disturb the mode"
        );
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
