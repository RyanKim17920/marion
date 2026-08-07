//! `CSI 3J` suppression as a [`Handler`] wrapper.
//!
//! Not a byte filter. §5.3 and S11 both rule that out: `ESC`, `[`, `3`, `J` can arrive in four
//! separate `read()`s, so a filter would have to reimplement the state machine it sits in front
//! of — and would still be wrong about `CSI 3;2J`, which parses as two parameters. Overriding the
//! one `Handler` method the sequence dispatches to is the only place the decision is unambiguous.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::Term as ATerm;
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, NamedPrivateMode,
    PrivateMode, Rgb, ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
};

use alacritty_terminal::vte::ansi::cursor_icon::CursorIcon;

use crate::Stats;

/// Wraps alacritty's `Term` for the duration of one `advance` call.
///
/// Overrides one method behaviourally and three observationally, delegating the rest verbatim:
/// * `clear_screen(ClearMode::Saved)` — swallowed, so marion's history outlives the display.
/// * `unset_private_mode(SyncUpdate)` — the DECSET 2026 frame boundary, counted then delegated.
/// * `goto` / `goto_line` — recorded only so a test can show frames without a CUP exist.
/// * `set_scrolling_region` — recorded only so a test can show *why* `vt100` drops the history
///   alacritty keeps.
pub struct Suppressor<'a> {
    pub(crate) term: &'a mut ATerm<VoidListener>,
    pub(crate) stats: &'a mut Stats,
    pub(crate) suppress: bool,
}

/// Forward a `Handler` method to the wrapped terminal unchanged.
macro_rules! delegate {
    ($( fn $name:ident ( $( $arg:ident : $ty:ty ),* ); )*) => {
        $(
            #[inline]
            fn $name(&mut self, $( $arg : $ty ),*) {
                self.term.$name( $( $arg ),* )
            }
        )*
    };
}

impl Handler for Suppressor<'_> {
    /// The one behavioural override. `CSI 3J` is *erase scrollback*; alacritty honours it with
    /// `Grid::clear_history()`. The harness clears scrollback because it is about to repaint a
    /// viewport, not because the transcript is invalid.
    #[inline]
    fn clear_screen(&mut self, mode: ClearMode) {
        if matches!(mode, ClearMode::Saved) {
            if self.suppress {
                self.stats.suppressed_erase_saved += 1;
                return;
            }
            self.stats.honoured_erase_saved += 1;
        }
        self.term.clear_screen(mode)
    }

    /// Start of a synchronized-output bracket.
    #[inline]
    fn set_private_mode(&mut self, mode: PrivateMode) {
        if mode == PrivateMode::Named(NamedPrivateMode::SyncUpdate) {
            self.stats.in_frame = true;
            self.stats.cup_in_frame = false;
        }
        self.term.set_private_mode(mode)
    }

    /// End of a synchronized-output bracket: one complete frame has been applied to the grid.
    ///
    /// **Edge-triggered, and it has to be.** vte 0.15 calls `unset_private_mode(SyncUpdate)` a
    /// number of times that depends on how the pty bytes were *chunked*: replaying
    /// `claude-2.1.220-boot-exit` whole yields 9 calls for its 8 brackets, replaying it one byte
    /// at a time yields 16, because `Processor::stop_sync_internal` both re-parses the buffered
    /// ESU and then reports the mode itself. Counting calls would make the frame count a function
    /// of `read()` sizes — precisely the failure S11's MUST #1 forbids.
    #[inline]
    fn unset_private_mode(&mut self, mode: PrivateMode) {
        if mode == PrivateMode::Named(NamedPrivateMode::SyncUpdate) && self.stats.in_frame {
            self.stats.in_frame = false;
            self.stats.frames += 1;
            if self.stats.cup_in_frame {
                self.stats.frames_with_cup += 1;
            }
        }
        self.term.unset_private_mode(mode)
    }

    #[inline]
    fn goto(&mut self, line: i32, col: usize) {
        self.stats.cup_in_frame = true;
        self.term.goto(line, col)
    }

    #[inline]
    fn goto_line(&mut self, line: i32) {
        self.stats.cup_in_frame = true;
        self.term.goto_line(line)
    }

    /// DECSTBM. Recorded, never altered — see [`Stats::top_anchored_partial_regions`].
    ///
    /// `top` is the 1-based parameter and `bottom` is `None` for the parameterless reset, which is
    /// full-height by definition.
    #[inline]
    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        if top <= 1 && bottom.is_some_and(|b| b < self.term.screen_lines()) {
            self.stats.top_anchored_partial_regions += 1;
        }
        self.term.set_scrolling_region(top, bottom)
    }

    delegate! {
        fn set_title(title: Option<String>);
        fn set_cursor_style(style: Option<CursorStyle>);
        fn set_cursor_shape(shape: CursorShape);
        fn input(c: char);
        fn goto_col(col: usize);
        fn insert_blank(count: usize);
        fn move_up(count: usize);
        fn move_down(count: usize);
        fn identify_terminal(intermediate: Option<char>);
        fn device_status(arg: usize);
        fn move_forward(col: usize);
        fn move_backward(col: usize);
        fn move_down_and_cr(row: usize);
        fn move_up_and_cr(row: usize);
        fn put_tab(count: u16);
        fn backspace();
        fn carriage_return();
        fn linefeed();
        fn bell();
        fn substitute();
        fn newline();
        fn set_horizontal_tabstop();
        fn scroll_up(count: usize);
        fn scroll_down(count: usize);
        fn insert_blank_lines(count: usize);
        fn delete_lines(count: usize);
        fn erase_chars(count: usize);
        fn delete_chars(count: usize);
        fn move_backward_tabs(count: u16);
        fn move_forward_tabs(count: u16);
        fn save_cursor_position();
        fn restore_cursor_position();
        fn clear_line(mode: LineClearMode);
        fn clear_tabs(mode: TabulationClearMode);
        fn set_tabs(interval: u16);
        fn reset_state();
        fn reverse_index();
        fn terminal_attribute(attr: Attr);
        fn set_mode(mode: Mode);
        fn unset_mode(mode: Mode);
        fn report_mode(mode: Mode);
        fn report_private_mode(mode: PrivateMode);
        fn set_keypad_application_mode();
        fn unset_keypad_application_mode();
        fn set_active_charset(index: CharsetIndex);
        fn configure_charset(index: CharsetIndex, charset: StandardCharset);
        fn set_color(index: usize, color: Rgb);
        fn dynamic_color_sequence(prefix: String, index: usize, terminator: &str);
        fn reset_color(index: usize);
        fn clipboard_store(clipboard: u8, base64: &[u8]);
        fn clipboard_load(clipboard: u8, terminator: &str);
        fn decaln();
        fn push_title();
        fn pop_title();
        fn text_area_size_pixels();
        fn text_area_size_chars();
        fn set_hyperlink(hyperlink: Option<Hyperlink>);
        fn set_mouse_cursor_icon(icon: CursorIcon);
        fn report_keyboard_mode();
        fn push_keyboard_mode(mode: KeyboardModes);
        fn pop_keyboard_modes(to_pop: u16);
        fn set_keyboard_mode(mode: KeyboardModes, behavior: KeyboardModesApplyBehavior);
        fn set_modify_other_keys(mode: ModifyOtherKeys);
        fn report_modify_other_keys();
        fn set_scp(char_path: ScpCharPath, update_mode: ScpUpdateMode);
    }
}
