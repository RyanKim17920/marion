//! **Text a node wrote, made safe to show on the operator's terminal.**
//!
//! A child's narrative, a model's reply and a tool's output are arbitrary text, and a terminal
//! treats some of it as instructions: an OSC 52 sequence writes the clipboard, OSC 8 hides a link
//! behind other words, OSC 0 retitles the window, a bare CSI moves the cursor over what was
//! printed. So every path where such text reaches the operator's terminal, or is typed into a
//! pane, passes through [`printable`] first.

use std::borrow::Cow;

use marion_core::text::{is_deceptive, visible};
pub use marion_core::text::{visible_line, visible_lines};

/// **Text the CLI composes over several lines, as the operator's terminal shows it**: `text`
/// written to `w` as [`visible_lines`] spells it, so its own line breaks and tabs stay and a
/// control, a bidi override or a zero-width character reads as its escape. For a usage, a report,
/// a detail the CLI lays out itself — text whose line breaks are the CLI's. A line that formats a
/// value prints with [`out!`](crate::out), [`err!`](crate::err) or [`out_to!`](crate::out_to),
/// which keep it one line; the three-axis `raw-print` rule keeps a raw print from coming back.
pub fn show(w: &mut dyn std::io::Write, text: &str) -> std::io::Result<()> {
    w.write_all(visible_lines(text).as_bytes())
}

/// [`show`] `text` and a newline to stdout: text the CLI composed over several lines (a help, a
/// usage, a scoreboard), as `println!("{text}")` printed it. A closed stdout is not an error the
/// caller can act on.
pub fn out_text(text: &str) {
    let _ = show(&mut std::io::stdout(), &format!("{text}\n"));
}

/// [`out_text`] to stderr.
pub fn err_text(text: &str) {
    let _ = show(&mut std::io::stderr(), &format!("{text}\n"));
}

/// `text` as **one** line and its newline, every control in it spelled but a tab
/// ([`visible_line`]): a value that carries a line break cannot add a row the CLI did not write.
pub fn line(text: &str) -> String {
    format!("{}\n", visible_line(text))
}

/// `println!` as one [`line`] to stdout. A closed stdout is not an error the caller can act on.
#[macro_export]
macro_rules! out {
    () => { $crate::out!("") };
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::io::stdout()
            .write_all($crate::printable::line(&::std::format!($($arg)*)).as_bytes());
    }};
}

/// `eprintln!` as one [`line`] to stderr.
#[macro_export]
macro_rules! err {
    () => { $crate::err!("") };
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::io::stderr()
            .write_all($crate::printable::line(&::std::format!($($arg)*)).as_bytes());
    }};
}

/// `writeln!(w, …)` as one [`line`]: the same `io::Result<()>`, and `w` taken as `writeln!` takes
/// it (a `&mut dyn Write`, a `&mut Vec<u8>` or a writer by value).
#[macro_export]
macro_rules! out_to {
    ($w:expr) => { $crate::out_to!($w, "") };
    ($w:expr, $($arg:tt)*) => {{
        // Unused where `w` is a `dyn Write`, whose methods need no import.
        #[allow(unused_imports)]
        use ::std::io::Write as _;
        $w.write_all($crate::printable::line(&::std::format!($($arg)*)).as_bytes())
    }};
}

/// `text` with every control character removed except newline and tab, and a CR (alone or before
/// an LF) as the newline it means. C1 controls (U+0080–U+009F, which include an 8-bit CSI) are
/// control characters too. What remains of an escape sequence is its printable tail, shown as
/// plain text, never acted on. A bidi override or a zero-width character
/// ([`marion_core::text::is_deceptive`] and not a control) is spelled out instead, as
/// [`marion_core::text::visible`] spells it: dropped, the text would read as if it never had one.
/// Borrowed where there was nothing to change.
pub fn printable(text: &str) -> Cow<'_, str> {
    if !text
        .chars()
        .any(|c| is_deceptive(c) && c != '\n' && c != '\t')
    {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c if is_deceptive(c) => out.push_str(&visible(c.encode_utf8(&mut [0; 4]))),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **No escape a node writes survives**: OSC 52 (clipboard), OSC 8 (hidden link), OSC 0
    /// (title), CSI and an 8-bit CSI all lose their introducers, newline and tab stay, and a CR
    /// becomes a newline. Clean text is borrowed untouched.
    #[test]
    fn every_terminal_escape_loses_its_control_characters() {
        for (raw, want) in [
            ("\x1b]52;c;cHduZWQ=\x07done", "]52;c;cHduZWQ=done"),
            (
                "\x1b]8;;https://evil.example\x1b\\click\x1b]8;;\x1b\\",
                "]8;;https://evil.example\\click]8;;\\",
            ),
            ("\x1b]0;owned\x07title", "]0;ownedtitle"),
            ("a\x1b[2Jb\u{9b}31mc", "a[2Jb31mc"),
            ("one\r\ntwo\rthree\tfour\n", "one\ntwo\nthree\tfour\n"),
        ] {
            let got = printable(raw);
            assert_eq!(got, want, "{raw:?}");
            assert!(
                !got.chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t')
            );
        }
        assert!(matches!(printable("plain\ttext\n"), Cow::Borrowed(_)));
    }

    /// **A bidi override or a zero-width character is spelled, not kept or dropped**: kept, it
    /// reorders or hides what the operator reads; dropped, the text looks as if it never had one.
    #[test]
    fn a_bidi_or_zero_width_character_is_spelled_out() {
        assert_eq!(
            printable("abc\u{202e}fed\u{200b}x\x1b"),
            "abc\\u{202e}fed\\u{200b}x"
        );
    }

    /// **A CLI print is one line, whatever its values carry**: an ESC, a CR, a bidi override or a
    /// line break in a value reads as its escape, so a value cannot add a row of its own; a tab
    /// stays, and text the CLI composes over several lines goes through [`show`], which keeps them.
    #[test]
    fn a_cli_print_is_one_line_with_every_control_spelled() {
        let mut buf = Vec::new();
        let value = "a\x1b[2Kb\rc\u{202e}d\nmarion: trusted";
        crate::out_to!(&mut buf, "{value}\t|{}", "e").unwrap();
        crate::out_to!(&mut buf).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "a\\x1b[2Kb\\rc\\u{202e}d\\nmarion: trusted\t|e\n\n"
        );
        let mut buf = Vec::new();
        show(&mut buf, "usage: one\n       two\x1b\n").unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "usage: one\n       two\\x1b\n"
        );
    }
}
