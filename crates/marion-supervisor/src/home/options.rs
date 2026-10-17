//! **The options typed before a task on the `n` line**, split off as `marion run`'s argv words.
//!
//! `n` then `--detach --timeout 1200 add rate limiting` is `marion run <type> --detach --timeout
//! 1200 --prompt "add rate limiting"`. Splitting needs only which flags take a value
//! ([`crate::run::RUN_FLAGS`]); what a flag means, and whether it is one at all, is `marion run`'s
//! own parser's to say when the line runs, so a flag it refuses is refused in its words. Quoting is
//! a shell's, for a value with spaces (`--verify "cargo test -q"`), and `--` ends the options so a
//! task may begin with a dash.

/// The option words before the task, and the task: everything after them, as typed.
pub fn split(line: &str) -> Result<(Vec<String>, String), String> {
    let mut words = Vec::new();
    let mut rest = line.trim_start();
    while rest.starts_with("--") {
        let (word, after) = token(rest)?;
        rest = after.trim_start();
        if word == "--" {
            break;
        }
        let takes_value = !word.contains('=')
            && crate::run::RUN_FLAGS
                .iter()
                .any(|(flag, value)| *flag == word && *value);
        words.push(word);
        if takes_value {
            if rest.is_empty() {
                return Err(format!(
                    "{} needs a value",
                    words.last().expect("just pushed")
                ));
            }
            let (value, after) = token(rest)?;
            words.push(value);
            rest = after.trim_start();
        }
    }
    Ok((words, rest.trim_end().to_string()))
}

/// One shell word from the start of `s` (which starts with one), and what follows it: `'…'` taken
/// as it is, `"…"` and bare text with `\` escaping the next character.
fn token(s: &str) -> Result<(String, &str), String> {
    let mut out = String::new();
    let mut chars = s.char_indices();
    let mut quote: Option<char> = None;
    while let Some((i, c)) = chars.next() {
        match (quote, c) {
            (None, c) if c.is_whitespace() => return Ok((out, &s[i..])),
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => out.push(c),
            (_, '\\') => match chars.next() {
                Some((_, next)) => out.push(next),
                None => out.push('\\'),
            },
            (_, c) => out.push(c),
        }
    }
    match quote {
        Some(q) => Err(format!("a {q} is not closed in the options")),
        None => Ok((out, "")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(line: &str) -> (Vec<String>, String) {
        split(line).unwrap_or_else(|e| panic!("{line:?}: {e}"))
    }

    #[test]
    fn a_line_without_options_is_all_task() {
        assert_eq!(
            ok("add rate limiting"),
            (vec![], "add rate limiting".into())
        );
        assert_eq!(
            ok("  don't -break  it  "),
            (vec![], "don't -break  it".into()),
            "the task is kept as typed, apostrophes, dashes and inner spaces too"
        );
    }

    #[test]
    fn leading_flags_are_split_by_whether_they_take_a_value() {
        assert_eq!(
            ok("--detach --timeout 1200 add rate limiting"),
            (
                vec!["--detach".into(), "--timeout".into(), "1200".into()],
                "add rate limiting".into()
            )
        );
        assert_eq!(
            ok(r#"--verify "cargo test -q" --verify='make lint' fix it"#),
            (
                vec![
                    "--verify".into(),
                    "cargo test -q".into(),
                    "--verify=make lint".into()
                ],
                "fix it".into()
            )
        );
    }

    #[test]
    fn a_double_dash_ends_the_options_so_a_task_may_start_with_one() {
        assert_eq!(
            ok("--detach -- --help is broken"),
            (vec!["--detach".into()], "--help is broken".into())
        );
    }

    /// A flag `marion run` does not take is passed on as a word, for its parser to refuse in its
    /// own words; this only has to know where the flags end.
    #[test]
    fn an_unknown_flag_is_passed_on_for_marion_run_to_judge() {
        assert_eq!(
            ok("--frobnicate do it"),
            (vec!["--frobnicate".into()], "do it".into())
        );
    }

    #[test]
    fn a_missing_value_or_an_open_quote_is_refused_here() {
        assert_eq!(split("--timeout").unwrap_err(), "--timeout needs a value");
        assert!(
            split(r#"--verify "cargo test do it"#)
                .unwrap_err()
                .contains("not closed")
        );
        assert_eq!(ok("--detach"), (vec!["--detach".into()], String::new()));
    }
}
