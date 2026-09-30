//! Words a person reads, formatted the one way every surface prints them: the supervisor's replies
//! and the TUI's rows both go through these, so the two cannot spell a duration or a shell word
//! differently. Pure `String` in, `String` out; nothing here knows about a terminal.

/// One argument as a POSIX shell would need it typed: bare when it is made only of characters no
/// shell treats specially, else in double quotes with `"`, `\\`, `$` and `` ` `` escaped.
pub fn shell_word(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
    if plain {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    for c in arg.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// A duration in seconds the way a row prints it: `42s`, `2m17s`, `1h04m`, `2d03h`. The unit
/// below the largest is zero-padded so a ticking column does not jitter.
pub fn elapsed(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        3600..=86_399 => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
        _ => format!("{}d{:02}h", secs / 86_400, (secs % 86_400) / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_words_quote_only_what_a_shell_would_mangle() {
        assert_eq!(shell_word("claude"), "claude");
        assert_eq!(shell_word("--model"), "--model");
        assert_eq!(shell_word("a b"), "\"a b\"");
        assert_eq!(shell_word("say \"hi\" $HOME"), "\"say \\\"hi\\\" \\$HOME\"");
        assert_eq!(shell_word(""), "\"\"");
        assert_eq!(shell_word("<type>"), "\"<type>\"");
    }

    #[test]
    fn elapsed_reads_like_a_timer() {
        assert_eq!(elapsed(42), "42s");
        assert_eq!(elapsed(137), "2m17s");
        assert_eq!(elapsed(3840), "1h04m");
        assert_eq!(elapsed(2 * 86_400 + 3 * 3600), "2d03h");
    }
}
