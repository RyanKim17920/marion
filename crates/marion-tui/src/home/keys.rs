//! Raw terminal bytes to the keys the home screen answers to.
//!
//! The screen reads stdin raw, so a key is whatever bytes the terminal sent for it: a printable
//! character as its UTF-8, an arrow as `ESC [ A`, Page Up as `ESC [ 5 ~`. A bracketed paste
//! (`ESC [ 200 ~` … `ESC [ 201 ~`) is one [`Key::Paste`], its line breaks flattened to spaces, so
//! pasting a multi-line prompt does not press Enter halfway through it.
//!
//! A lone `ESC` at the end of a chunk is [`Key::Esc`]: terminals send an escape sequence in one
//! write, so an `ESC` with nothing after it is the key itself. Sequences this screen has no use for
//! are skipped whole rather than read as text.

/// One key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// `^` plus a letter, as its lowercase letter (`Ctrl('o')`). Not Tab, Enter or Backspace,
    /// which have their own variants.
    Ctrl(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Paste(String),
}

/// Decode a chunk of stdin into keys.
pub fn decode(bytes: &[u8]) -> Vec<Key> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let (key, used) = next(&bytes[i..]);
        out.extend(key);
        i += used.max(1);
    }
    out
}

fn next(b: &[u8]) -> (Option<Key>, usize) {
    const PASTE_START: &[u8] = b"\x1b[200~";
    const PASTE_END: &[u8] = b"\x1b[201~";
    if b.starts_with(PASTE_START) {
        let body = &b[PASTE_START.len()..];
        let end = body
            .windows(PASTE_END.len())
            .position(|w| w == PASTE_END)
            .unwrap_or(body.len());
        let text: String = String::from_utf8_lossy(&body[..end])
            .chars()
            .map(|c| {
                if c == '\r' || c == '\n' || c == '\t' {
                    ' '
                } else {
                    c
                }
            })
            .filter(|c| !c.is_control())
            .collect();
        let used = PASTE_START.len() + (end + PASTE_END.len()).min(body.len());
        return (Some(Key::Paste(text)), used);
    }
    match b[0] {
        0x1b => escape(b),
        b'\r' | b'\n' => (Some(Key::Enter), 1),
        b'\t' => (Some(Key::Tab), 1),
        0x7f | 0x08 => (Some(Key::Backspace), 1),
        c @ 0x01..=0x1a => (Some(Key::Ctrl((b'a' + c - 1) as char)), 1),
        0x00..=0x1f => (None, 1),
        _ => {
            let len = match b[0] {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => 1,
            };
            let end = len.min(b.len());
            match std::str::from_utf8(&b[..end])
                .ok()
                .and_then(|s| s.chars().next())
            {
                Some(c) => (Some(Key::Char(c)), end),
                None => (None, end),
            }
        }
    }
}

/// An escape sequence, or the Esc key itself.
fn escape(b: &[u8]) -> (Option<Key>, usize) {
    match b.get(1) {
        None => (Some(Key::Esc), 1),
        // `ESC ESC`: the first is the key.
        Some(0x1b) => (Some(Key::Esc), 1),
        Some(b'[') | Some(b'O') => {
            // CSI / SS3: parameters, then one final byte in `@`..=`~`.
            let Some(fin) = b[2..].iter().position(|c| (0x40..=0x7e).contains(c)) else {
                return (None, b.len());
            };
            let used = 2 + fin + 1;
            let params = &b[2..2 + fin];
            let key = match (b[2 + fin], params) {
                (b'A', _) => Some(Key::Up),
                (b'B', _) => Some(Key::Down),
                (b'C', _) => Some(Key::Right),
                (b'D', _) => Some(Key::Left),
                (b'H', _) => Some(Key::Home),
                (b'F', _) => Some(Key::End),
                (b'Z', _) => Some(Key::BackTab),
                (b'~', b"5") => Some(Key::PageUp),
                (b'~', b"6") => Some(Key::PageDown),
                (b'~', b"1" | b"7") => Some(Key::Home),
                (b'~', b"4" | b"8") => Some(Key::End),
                _ => None,
            };
            (key, used)
        }
        // `ESC x` (Alt+x): read as Esc then x, which is what a user pressing Esc quickly meant.
        Some(_) => (Some(Key::Esc), 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_text_and_utf8_are_characters() {
        assert_eq!(decode(b"ab"), [Key::Char('a'), Key::Char('b')]);
        assert_eq!(decode("é日".as_bytes()), [Key::Char('é'), Key::Char('日')]);
    }

    #[test]
    fn control_keys_have_their_own_names() {
        assert_eq!(
            decode(b"\r\t\x7f\x0f\x03"),
            [
                Key::Enter,
                Key::Tab,
                Key::Backspace,
                Key::Ctrl('o'),
                Key::Ctrl('c')
            ]
        );
    }

    #[test]
    fn escape_sequences_are_one_key_each() {
        assert_eq!(
            decode(b"\x1b[A\x1b[B\x1b[C\x1b[D\x1b[5~\x1b[6~\x1b[Z\x1bOA"),
            [
                Key::Up,
                Key::Down,
                Key::Right,
                Key::Left,
                Key::PageUp,
                Key::PageDown,
                Key::BackTab,
                Key::Up
            ]
        );
        assert_eq!(decode(b"\x1b"), [Key::Esc]);
        assert_eq!(
            decode(b"\x1b[99x"),
            [],
            "an unknown sequence is skipped, not typed"
        );
    }

    #[test]
    fn a_bracketed_paste_is_one_key_with_its_newlines_flattened() {
        assert_eq!(
            decode(b"\x1b[200~line one\nline two\x1b[201~x"),
            [Key::Paste("line one line two".into()), Key::Char('x')]
        );
    }
}
