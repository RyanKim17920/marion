//! **Text a node wrote, made safe to show on the operator's terminal.**
//!
//! A child's narrative, a model's reply and a tool's output are arbitrary text, and a terminal
//! treats some of it as instructions: an OSC 52 sequence writes the clipboard, OSC 8 hides a link
//! behind other words, OSC 0 retitles the window, a bare CSI moves the cursor over what was
//! printed. So every path where such text reaches the operator's terminal, or is typed into a
//! pane, passes through [`printable`] first.

use std::borrow::Cow;

/// `text` with every control character removed except newline and tab, and a CR (alone or before
/// an LF) as the newline it means. C1 controls (U+0080–U+009F, which include an 8-bit CSI) are
/// control characters too. What remains of an escape sequence is its printable tail, shown as
/// plain text, never acted on. Borrowed where there was nothing to remove.
pub fn printable(text: &str) -> Cow<'_, str> {
    if !text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
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
}
