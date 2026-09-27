//! Column arithmetic for the home screen: every row goes through these, so a narrow terminal
//! loses the least important words first and never a column boundary.
//!
//! **Measured in display columns, not characters.** [`crate::tree::clip`] counts `char`s, which is
//! right for the tree column's ASCII labels and wrong for a prompt an operator typed in CJK or with
//! an emoji in it: two columns per character would push the right-hand group off the edge. These
//! helpers use `unicode-width`, which is what `ratatui` itself lays cells out by.
//!
//! The rules, in the order a row is built:
//! * [`clip`] — at most `w` columns, a trailing `…` when anything was cut.
//! * [`pad`] / [`rpad`] — clip, then pad to exactly `w`: the fixed-width field that keeps columns
//!   straight.
//! * [`fit`] — a run of spans clipped to `w`, the crossing span ellipsised.
//! * [`lr`] — a left group and a right group pinned to the right edge; the left is clipped first,
//!   so the right-hand fact (a state, a count, an elapsed time) never falls off.
//! * [`fit_groups`] — whole groups only, dropped from the end: a half hint is worse than none.
//! * [`wrap`], [`kv_lines`], [`squeeze`] — for the few places that take more than one row.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display columns `s` occupies.
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Display columns a run of spans occupies.
pub fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| width(&s.content)).sum()
}

/// `s` if it fits in `w` columns, else as much of it as fits in `w - 1` columns and `…`.
///
/// A wide character that would straddle the cut is dropped whole rather than split, so the result
/// can be one column short of `w`; [`pad`] fills that column back in.
pub fn clip(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > w - 1 {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

/// [`clip`] from the other end: the last `w - 1` columns of `s` after a leading `…`. For text being
/// typed, where the end is the part that matters.
pub fn clip_left(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut tail = Vec::new();
    let mut used = 0;
    for ch in s.chars().rev() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > w - 1 {
            break;
        }
        tail.push(ch);
        used += cw;
    }
    std::iter::once('…').chain(tail.into_iter().rev()).collect()
}

/// Clip, then right-pad with spaces to exactly `w` columns.
pub fn pad(s: &str, w: usize) -> String {
    let c = clip(s, w);
    let n = w.saturating_sub(width(&c));
    format!("{c}{}", " ".repeat(n))
}

/// Clip, then left-pad with spaces to exactly `w` columns: right-aligned numbers.
pub fn rpad(s: &str, w: usize) -> String {
    let c = clip(s, w);
    let n = w.saturating_sub(width(&c));
    format!("{}{c}", " ".repeat(n))
}

/// Clip a run of spans to `w` columns, ellipsising the span that crosses the edge and dropping
/// everything after it. Styles are kept, so the `…` wears the style of the text it replaced.
pub fn fit<'a>(spans: Vec<Span<'a>>, w: usize) -> Vec<Span<'a>> {
    if spans_width(&spans) <= w {
        return spans;
    }
    let mut out = Vec::new();
    let mut used = 0;
    for s in spans {
        let sw = width(&s.content);
        if used + sw < w {
            used += sw;
            out.push(s);
        } else {
            let room = w - used;
            if room > 0 {
                // Something is always cut here — this span or the ones after it — so the `…` goes
                // in even when this span alone would have fitted exactly.
                out.push(Span::styled(
                    clip(&format!("{}…", s.content), room),
                    s.style,
                ));
            }
            break;
        }
    }
    out
}

/// `left`, then `right` pinned to the right edge of a `w`-column row.
///
/// The left group is clipped first and always leaves one column of air before the right group, so
/// the two never read as one word. If the right group alone is wider than `w`, it is clipped too.
pub fn lr<'a>(left: Vec<Span<'a>>, right: Vec<Span<'a>>, w: usize) -> Line<'a> {
    let right = fit(right, w);
    let rw = spans_width(&right);
    let room = w.saturating_sub(rw + 1);
    let mut l = if room == 0 {
        Vec::new()
    } else {
        fit(left, room)
    };
    let lw = spans_width(&l);
    l.push(Span::raw(" ".repeat(w.saturating_sub(lw + rw))));
    l.extend(right);
    Line::from(l)
}

/// Whole groups, in order, while they fit in `w` columns; the first group that does not fit and
/// every group after it are dropped.
pub fn fit_groups<'a>(groups: Vec<Vec<Span<'a>>>, w: usize) -> Vec<Span<'a>> {
    let mut out = Vec::new();
    let mut used = 0;
    for g in groups {
        let gw = spans_width(&g);
        if used + gw > w {
            break;
        }
        used += gw;
        out.extend(g);
    }
    out
}

/// Greedy word wrap to `w` columns. A single word wider than `w` is clipped rather than split, and
/// an empty or all-space `s` wraps to no lines.
pub fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let need = if cur.is_empty() {
            width(word)
        } else {
            width(&cur) + 1 + width(word)
        };
        if need > w && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(&clip(word, w));
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// A label and its value on one row when they fit in `w` columns; otherwise the label on one row
/// and the value, indented two columns and clipped, on the next.
pub fn kv_lines<'a>(
    label: &str,
    label_w: usize,
    value: Vec<Span<'a>>,
    label_style: Style,
    w: usize,
) -> Vec<Line<'a>> {
    if label_w + spans_width(&value) <= w {
        let mut s = vec![Span::styled(pad(label, label_w), label_style)];
        s.extend(value);
        vec![Line::from(s)]
    } else {
        let mut v = vec![Span::raw("  ")];
        v.extend(value);
        vec![
            Line::from(Span::styled(clip(label, w), label_style)),
            Line::from(fit(v, w)),
        ]
    }
}

/// Lines where `None` is a spacer that may be dropped: spacers go from the bottom up until the
/// lines fit in `rows`, and only then is content truncated.
pub fn squeeze<'a>(mut lines: Vec<Option<Line<'a>>>, rows: usize) -> Vec<Line<'a>> {
    while lines.len() > rows {
        match lines.iter().rposition(Option::is_none) {
            Some(i) => {
                lines.remove(i);
            }
            None => break,
        }
    }
    lines
        .into_iter()
        .map(Option::unwrap_or_default)
        .take(rows)
        .collect()
}

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

/// `argv` as one shell line in at most `w` columns. When it is too wide, the longest argument —
/// in practice the prompt — is clipped **inside its quotes**, so the line still reads as a command
/// an operator could finish typing; only if that is not enough is the whole line clipped.
pub fn shell_line(argv: &[String], w: usize) -> String {
    let words: Vec<String> = argv.iter().map(|a| shell_word(a)).collect();
    let line = words.join(" ");
    if width(&line) <= w {
        return line;
    }
    let Some((longest, _)) = argv.iter().enumerate().max_by_key(|(_, a)| width(a)) else {
        return clip(&line, w);
    };
    let others: usize = words
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != longest)
        .map(|(_, word)| width(word) + 1)
        .sum();
    // Two quotes, and the room the rest of the line leaves.
    let room = w.saturating_sub(others + 2);
    if room < 2 {
        return clip(&line, w);
    }
    let mut words = words;
    let inner = shell_word(&argv[longest]);
    let inner = inner.trim_start_matches('"').trim_end_matches('"');
    words[longest] = format!("\"{}\"", clip(inner, room));
    clip(&words.join(" "), w)
}

/// A token count the way every row prints one: `812`, `88k`, `184.2k`, `1.3M`.
///
/// One decimal below 100k when it says something (`12.9k`), none from 100k up, where the decimal
/// is noise in a column, nor when the count is round (`88k`, not `88.0k`). Tokens only:
/// marion never prices a run, because it cannot know what the operator's plan charges.
pub fn tokens(n: u64) -> String {
    /// `tenths` of a `unit`, without a `.0`.
    fn tenths(t: u64, unit: &str) -> String {
        if t.is_multiple_of(10) {
            format!("{}{unit}", t / 10)
        } else {
            format!("{}.{}{unit}", t / 10, t % 10)
        }
    }
    match n {
        0..=999 => n.to_string(),
        // Below 99.95k, rounding to a tenth stays under `100.0k`.
        1_000..=99_949 => tenths((n + 50) / 100, "k"),
        99_950..=999_499 => format!("{}k", (n + 500) / 1000),
        _ => tenths((n + 50_000) / 100_000, "M"),
    }
}

/// A duration in seconds the way a row prints it: `42s`, `2m17s`, `1h04m`.
pub fn elapsed(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn clip_measures_columns_not_characters() {
        assert_eq!(clip("abcdef", 6), "abcdef");
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("abcdef", 0), "");
        assert_eq!(clip("abcdef", 1), "…");
        // Each of these is two columns wide: four characters are eight columns.
        assert_eq!(clip("日本語版", 8), "日本語版");
        assert_eq!(width(&clip("日本語版", 6)), 5, "{}", clip("日本語版", 6));
        assert_eq!(clip("日本語版", 6), "日本…");
    }

    #[test]
    fn clip_left_keeps_the_end() {
        assert_eq!(clip_left("abcdef", 6), "abcdef");
        assert_eq!(clip_left("abcdef", 4), "…def");
        assert_eq!(clip_left("abcdef", 0), "");
        assert_eq!(clip_left("日本語版", 6), "…語版");
    }

    #[test]
    fn pad_and_rpad_always_fill_the_field() {
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(rpad("ab", 4), "  ab");
        assert_eq!(pad("abcdef", 4), "abc…");
        // A wide character that cannot straddle the cut is padded back to the full width.
        assert_eq!(width(&pad("日本語版", 6)), 6);
    }

    #[test]
    fn fit_ellipsises_the_crossing_span_and_keeps_its_style() {
        let bold = Style::default().add_modifier(ratatui::style::Modifier::BOLD);
        let out = fit(vec![Span::raw("abc"), Span::styled("defgh", bold)], 6);
        assert_eq!(text(&out), "abcde…");
        assert_eq!(out[1].style, bold);
        assert_eq!(fit(vec![Span::raw("abc")], 3).len(), 1);
        // Exactly at the edge: the crossing span is the next one, and it gets no room at all.
        assert_eq!(text(&fit(vec![Span::raw("abc"), Span::raw("d")], 3)), "ab…");
    }

    #[test]
    fn lr_never_loses_the_right_group() {
        let line = lr(
            vec![Span::raw("a long left-hand description")],
            vec![Span::raw("12m04s")],
            20,
        );
        let s: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(width(&s), 20);
        assert!(s.ends_with(" 12m04s"), "{s:?}");
        assert!(s.contains('…'), "{s:?}");
        let s: String = lr(vec![Span::raw("ab")], vec![Span::raw("cd")], 8)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(s, "ab    cd");
        // Right group wider than the row: clipped, never overflowing.
        let s: String = lr(vec![Span::raw("x")], vec![Span::raw("abcdefgh")], 4)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(width(&s), 4);
    }

    #[test]
    fn fit_groups_drops_whole_groups_from_the_end() {
        let g = |s: &str| vec![Span::raw(s.to_string())];
        let out = fit_groups(
            vec![g("j/k move"), g(" · enter attach"), g(" · m merge")],
            24,
        );
        assert_eq!(text(&out), "j/k move · enter attach");
        assert_eq!(text(&fit_groups(vec![g("j/k move")], 3)), "");
    }

    #[test]
    fn wrap_is_greedy_and_clips_overlong_words() {
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap("abcdefghij x", 4), vec!["abc…", "x"]);
        assert!(wrap("   ", 10).is_empty());
    }

    #[test]
    fn kv_lines_stacks_when_the_pair_does_not_fit() {
        let one = kv_lines("FIX", 6, vec![Span::raw("brew up")], Style::default(), 20);
        assert_eq!(one.len(), 1);
        let two = kv_lines(
            "FIX",
            6,
            vec![Span::raw("a much longer fix line")],
            Style::default(),
            20,
        );
        assert_eq!(two.len(), 2);
        assert_eq!(width(&two[1].to_string()), 20);
    }

    #[test]
    fn squeeze_drops_spacers_before_content() {
        let l = |s: &'static str| Some(Line::from(s));
        let out = squeeze(vec![l("a"), None, l("b"), None, l("c")], 4);
        assert_eq!(out.len(), 4);
        assert_eq!(out[3].to_string(), "c");
        let out = squeeze(vec![l("a"), l("b"), l("c")], 2);
        assert_eq!(out.len(), 2);
    }

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
    fn a_long_prompt_is_clipped_inside_its_quotes() {
        let argv: Vec<String> = [
            "marion",
            "run",
            "claude",
            "--prompt",
            "Add rate limiting to /v1/orders",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            shell_line(&argv, 80),
            "marion run claude --prompt \"Add rate limiting to /v1/orders\""
        );
        let short = shell_line(&argv, 40);
        assert_eq!(width(&short), 40, "{short}");
        assert_eq!(short, "marion run claude --prompt \"Add rate l…\"");
    }

    #[test]
    fn tokens_read_like_a_column_of_counts() {
        assert_eq!(tokens(812), "812");
        assert_eq!(tokens(88_000), "88k");
        assert_eq!(tokens(184_210), "184k");
        assert_eq!(tokens(99_960), "100k");
        assert_eq!(tokens(12_949), "12.9k");
        assert_eq!(tokens(999_960), "1M");
        assert_eq!(tokens(999_400), "999k");
        assert_eq!(tokens(1_300_000), "1.3M");
        assert_eq!(tokens(151_000), "151k");
    }

    #[test]
    fn elapsed_reads_like_a_timer() {
        assert_eq!(elapsed(42), "42s");
        assert_eq!(elapsed(137), "2m17s");
        assert_eq!(elapsed(3840), "1h04m");
    }
}
