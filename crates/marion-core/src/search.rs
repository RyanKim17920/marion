//! **What a search across runs asks**, parsed: words, quoted phrases and `key:value` filters.
//!
//! ```text
//! limiter "token bucket" harness:codex state:failed since:2d in:report
//! ```
//!
//! - Words and phrases are ANDed, each matched case-insensitively as a substring.
//! - Filters: `harness:` `model:` `type:` `state:` `project:` `since:` `until:` `in:` `id:`.
//!   The same key given twice ORs (`harness:codex harness:claude`); different keys AND.
//! - An unknown filter is an error naming the valid ones, never a word, so a typo cannot quietly
//!   return nothing. A word that holds a colon is searched for by quoting it: `"a:b"`.
//!
//! One parser for the CLI and the TUI, and pure: "now" is handed in.

use std::fmt;

/// Every filter key, in the order an error lists them.
pub const FILTERS: &[&str] = &[
    "harness", "model", "type", "state", "project", "since", "until", "in", "id",
];

/// The states `state:` takes. `running` and `waiting` match only runs that are live.
pub const STATES: &[&str] = &[
    "running",
    "waiting",
    "ok",
    "failed",
    "cancelled",
    "killed",
    "timedout",
    "unreported",
    "landed",
];

/// The fields `in:` takes.
pub const SCOPES: &[&str] = &["meta", "prompt", "report", "paths", "all"];

/// A state a run is in or ended in, as `state:` spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Waiting,
    Ok,
    Failed,
    Cancelled,
    Killed,
    TimedOut,
    Unreported,
    /// Ended `ok` with a commit marion landed.
    Landed,
}

impl State {
    pub fn parse(word: &str) -> Option<State> {
        Some(match word {
            "running" => State::Running,
            "waiting" => State::Waiting,
            "ok" => State::Ok,
            "failed" => State::Failed,
            "cancelled" => State::Cancelled,
            "killed" => State::Killed,
            "timedout" => State::TimedOut,
            "unreported" => State::Unreported,
            "landed" => State::Landed,
            _ => return None,
        })
    }

    /// The word `state:` takes for this state, so what a row shows is what a filter types.
    pub fn word(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Waiting => "waiting",
            State::Ok => "ok",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
            State::Killed => "killed",
            State::TimedOut => "timedout",
            State::Unreported => "unreported",
            State::Landed => "landed",
        }
    }

    /// The state a run that ended with `status` is in.
    pub fn of_exit(status: crate::contract::ExitStatus) -> State {
        use crate::contract::ExitStatus as E;
        match status {
            E::Ok => State::Ok,
            E::Failed => State::Failed,
            E::Cancelled => State::Cancelled,
            E::Unreported => State::Unreported,
            E::TimedOut => State::TimedOut,
            E::Killed => State::Killed,
        }
    }
}

/// Which of a run's fields the words are matched in. All four by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scopes {
    /// Ids, agent type, harness, model, state and branch.
    pub meta: bool,
    pub prompt: bool,
    /// The report, the checks and the review's findings.
    pub report: bool,
    /// Changed paths and the branch.
    pub paths: bool,
}

impl Scopes {
    pub const ALL: Scopes = Scopes {
        meta: true,
        prompt: true,
        report: true,
        paths: true,
    };
    const NONE: Scopes = Scopes {
        meta: false,
        prompt: false,
        report: false,
        paths: false,
    };
}

/// A parsed search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// Words and phrases, as typed. Each must match somewhere in scope.
    pub terms: Vec<String>,
    pub harness: Vec<String>,
    pub model: Vec<String>,
    pub agent_type: Vec<String>,
    pub state: Vec<State>,
    pub project: Vec<String>,
    /// Prefixes of an agent or task id, or a short id.
    pub id: Vec<String>,
    /// Runs that ended at or after this, in unix milliseconds.
    pub since: Option<u64>,
    /// Runs that ended before this, in unix milliseconds.
    pub until: Option<u64>,
    pub scopes: Scopes,
}

/// Why a search did not parse. Its `Display` is the sentence the operator reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError(pub String);

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for QueryError {}

fn refuse<T>(why: String) -> Result<T, QueryError> {
    Err(QueryError(why))
}

impl Query {
    /// `text` as a query, `now_ms` being the instant `since:2d` counts back from.
    pub fn parse(text: &str, now_ms: u64) -> Result<Query, QueryError> {
        let mut q = Query::empty();
        let mut scopes: Option<Scopes> = None;
        for token in tokens(text)? {
            q.take(token, now_ms, &mut scopes)?;
        }
        q.scopes = scopes.unwrap_or(Scopes::ALL);
        Ok(q)
    }

    /// The words of a command line as a query: one shell argument holding whitespace and no
    /// quote is a phrase, as `marion search "token bucket"` means; any other is parsed as typed.
    pub fn parse_args(args: &[&str], now_ms: u64) -> Result<Query, QueryError> {
        let text: Vec<String> = args
            .iter()
            .map(|a| {
                if a.contains(char::is_whitespace) && !a.contains('"') && !is_filter(a) {
                    format!("\"{a}\"")
                } else {
                    a.to_string()
                }
            })
            .collect();
        Query::parse(&text.join(" "), now_ms)
    }

    fn empty() -> Query {
        Query {
            terms: Vec::new(),
            harness: Vec::new(),
            model: Vec::new(),
            agent_type: Vec::new(),
            state: Vec::new(),
            project: Vec::new(),
            id: Vec::new(),
            since: None,
            until: None,
            scopes: Scopes::ALL,
        }
    }

    /// Whether nothing narrows the search at all.
    pub fn is_empty(&self) -> bool {
        *self == Query::empty()
    }

    fn take(
        &mut self,
        token: Token,
        now_ms: u64,
        scopes: &mut Option<Scopes>,
    ) -> Result<(), QueryError> {
        let (key, value) = match token {
            Token::Term(t) => {
                self.terms.push(t);
                return Ok(());
            }
            Token::Filter(k, v) => (k, v),
        };
        if value.is_empty() {
            return refuse(format!("`{key}:` needs a value"));
        }
        let lower = value.to_lowercase();
        match key.as_str() {
            "harness" => self.harness.push(lower),
            "model" => self.model.push(lower),
            "type" => self.agent_type.push(lower),
            "project" => self.project.push(lower),
            "id" => self.id.push(lower),
            "state" => match State::parse(&lower) {
                Some(s) => self.state.push(s),
                None => {
                    return refuse(format!(
                        "`state:{value}` is not a state; use one of {}",
                        STATES.join(", ")
                    ));
                }
            },
            "since" => {
                let at = instant(&lower, now_ms, false)?;
                self.since = Some(self.since.map_or(at, |s| s.min(at)));
            }
            "until" => {
                let at = instant(&lower, now_ms, true)?;
                self.until = Some(self.until.map_or(at, |u| u.max(at)));
            }
            "in" => {
                let s = scopes.get_or_insert(Scopes::NONE);
                match lower.as_str() {
                    "meta" => s.meta = true,
                    "prompt" => s.prompt = true,
                    "report" => s.report = true,
                    "paths" => s.paths = true,
                    "all" => *s = Scopes::ALL,
                    "output" | "diff" => {
                        return refuse(format!(
                            "`in:{lower}` is not searched yet; search in {}",
                            SCOPES.join(", ")
                        ));
                    }
                    _ => {
                        return refuse(format!(
                            "`in:{value}` is not a field; use one of {}",
                            SCOPES.join(", ")
                        ));
                    }
                }
            }
            _ => {
                return refuse(format!(
                    "`{key}:` is not a filter; use one of {}, or quote the word (\"{key}:{value}\")",
                    FILTERS
                        .iter()
                        .map(|f| format!("{f}:"))
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Term(String),
    Filter(String, String),
}

/// `key:…` at the start of `word`, for any key, known or not.
fn is_filter(word: &str) -> bool {
    word.split_once(':')
        .is_some_and(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphabetic()))
}

/// The tokens of `text`: whitespace-separated words, `"…"` phrases, and `key:value` filters
/// whose value may itself be quoted.
fn tokens(text: &str) -> Result<Vec<Token>, QueryError> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&first) = chars.peek() else {
            return Ok(out);
        };
        if first == '"' {
            chars.next();
            let phrase = quoted(&mut chars)?;
            if !phrase.is_empty() {
                out.push(Token::Term(phrase));
            }
            continue;
        }
        let mut word = String::new();
        while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
            if c == '"' && word.ends_with(':') && is_filter(&word) {
                word.push_str(&quoted(&mut chars)?);
                break;
            }
            word.push(c);
        }
        match word.split_once(':') {
            Some((k, v)) if is_filter(&word) => {
                out.push(Token::Filter(k.to_ascii_lowercase(), v.to_string()))
            }
            _ => out.push(Token::Term(word)),
        }
    }
}

/// The rest of a phrase whose opening quote was read, through its closing one.
fn quoted(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<String, QueryError> {
    let mut phrase = String::new();
    for c in chars.by_ref() {
        if c == '"' {
            return Ok(phrase);
        }
        phrase.push(c);
    }
    refuse(format!("the phrase \"{phrase} has no closing quote"))
}

/// `30m 6h 2d 3w` back from `now_ms`, or a `2026-10-01` date (UTC): its start, or for `until:`
/// its end, so `until:2026-10-01` keeps the whole day.
fn instant(value: &str, now_ms: u64, end_of_day: bool) -> Result<u64, QueryError> {
    const UNITS: &[(char, u64)] = &[
        ('m', 60_000),
        ('h', 3_600_000),
        ('d', 86_400_000),
        ('w', 604_800_000),
    ];
    if let Some(unit) = value.chars().last()
        && let Some((_, ms)) = UNITS.iter().find(|(u, _)| *u == unit)
        && let Ok(n) = value[..value.len() - 1].parse::<u64>()
    {
        return Ok(now_ms.saturating_sub(n.saturating_mul(*ms)));
    }
    let stamp = format!("{value}T00:00:00.000Z");
    match serde_json::from_value::<crate::encoding::SystemTime>(serde_json::Value::String(stamp)) {
        Ok(t) if value.len() == 10 => {
            let ms =
                t.0.duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64);
            Ok(if end_of_day { ms + 86_400_000 } else { ms })
        }
        _ => refuse(format!(
            "`{value}` is not a time; use 30m, 6h, 2d, 3w or a date like 2026-10-01"
        )),
    }
}

/// **Whether `needle` occurs in `hay`, ignoring case**, and where: ASCII folded byte for byte, so
/// the offset is `hay`'s own; anything else through `to_lowercase`, with the offset kept only when
/// folding did not move it.
pub fn find_folded(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_ascii() {
        let (h, n) = (hay.as_bytes(), needle.as_bytes());
        if n.is_empty() {
            return Some(0);
        }
        return h.windows(n.len()).position(|w| w.eq_ignore_ascii_case(n));
    }
    let lower = hay.to_lowercase();
    let at = lower.find(&needle.to_lowercase())?;
    Some(if lower.len() == hay.len() && hay.is_char_boundary(at) {
        at
    } else {
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-16T00:00:00Z.
    const NOW: u64 = 1_792_108_800_000;

    fn q(text: &str) -> Query {
        Query::parse(text, NOW).unwrap()
    }

    fn err(text: &str) -> String {
        Query::parse(text, NOW).unwrap_err().0
    }

    #[test]
    fn words_and_phrases_are_terms() {
        let parsed = q(r#"limiter  "token bucket" ci"#);
        assert_eq!(parsed.terms, ["limiter", "token bucket", "ci"]);
        assert_eq!(parsed.scopes, Scopes::ALL);
        assert!(q("").is_empty());
    }

    #[test]
    fn each_filter_is_read() {
        let parsed = q(
            "harness:Codex model:gpt-5 type:codex-impl state:failed project:acme id:3c33 in:report",
        );
        assert_eq!(parsed.harness, ["codex"], "folded");
        assert_eq!(parsed.model, ["gpt-5"]);
        assert_eq!(parsed.agent_type, ["codex-impl"]);
        assert_eq!(parsed.state, [State::Failed]);
        assert_eq!(parsed.project, ["acme"]);
        assert_eq!(parsed.id, ["3c33"]);
        assert_eq!(
            parsed.scopes,
            Scopes {
                report: true,
                ..Scopes::NONE
            }
        );
        assert!(parsed.terms.is_empty());
    }

    #[test]
    fn the_same_key_twice_ors() {
        let parsed = q("harness:codex harness:claude state:ok state:landed in:prompt in:paths");
        assert_eq!(parsed.harness, ["codex", "claude"]);
        assert_eq!(parsed.state, [State::Ok, State::Landed]);
        assert!(parsed.scopes.prompt && parsed.scopes.paths);
        assert!(!parsed.scopes.meta && !parsed.scopes.report);
        assert_eq!(q("since:1d since:2d").since, Some(NOW - 2 * 86_400_000));
    }

    #[test]
    fn a_quoted_filter_value_keeps_its_spaces() {
        assert_eq!(q(r#"model:"gpt 5" x"#).model, ["gpt 5"]);
    }

    #[test]
    fn since_and_until_take_spans_and_dates() {
        assert_eq!(q("since:30m").since, Some(NOW - 30 * 60_000));
        assert_eq!(q("since:6h").since, Some(NOW - 6 * 3_600_000));
        assert_eq!(q("since:3w").since, Some(NOW - 21 * 86_400_000));
        assert_eq!(q("since:2026-10-16").since, Some(NOW));
        assert_eq!(q("until:2026-10-15").until, Some(NOW), "the whole day");
        for bad in [
            "since:2x",
            "since:yesterday",
            "until:2026-13-01x",
            "since:d",
        ] {
            assert!(err(bad).contains("is not a time"), "{bad}");
        }
    }

    /// Mutation: treat an unknown key as a word and a typo returns nothing, silently.
    #[test]
    fn an_unknown_filter_is_an_error_naming_the_valid_ones() {
        let e = err("hraness:codex");
        assert!(e.contains("`hraness:` is not a filter"), "{e}");
        for f in FILTERS {
            assert!(e.contains(&format!("{f}:")), "{e}");
        }
        assert_eq!(q(r#""a:b""#).terms, ["a:b"], "quoted, it is a word");
        assert_eq!(q("http//x 10:30").terms, ["http//x", "10:30"], "not a key");
    }

    #[test]
    fn a_bad_value_is_an_error() {
        assert!(err("state:sleepy").contains("running, waiting, ok"));
        assert!(err("in:everything").contains("meta, prompt, report, paths, all"));
        assert!(err("in:output").contains("not searched yet"));
        assert!(err("harness:").contains("needs a value"));
    }

    #[test]
    fn an_unbalanced_quote_is_an_error() {
        assert!(err(r#"limiter "token bucket"#).contains("no closing quote"));
        assert!(err(r#"model:"gpt"#).contains("no closing quote"));
    }

    #[test]
    fn a_shell_argument_with_spaces_is_a_phrase() {
        let parsed = Query::parse_args(&["token bucket", "harness:codex", "ci"], NOW).unwrap();
        assert_eq!(parsed.terms, ["token bucket", "ci"]);
        assert_eq!(parsed.harness, ["codex"]);
    }

    #[test]
    fn folded_find_ignores_case_and_keeps_offsets() {
        assert_eq!(find_folded("Add a Token-Bucket", "token-bucket"), Some(6));
        assert_eq!(find_folded("abc", "x"), None);
        assert_eq!(find_folded("Ünïcode Straße", "straße"), Some(10));
        assert_eq!(find_folded("ÉCOLE", "école"), Some(0));
    }

    #[test]
    fn every_state_word_round_trips() {
        for w in STATES {
            assert_eq!(State::parse(w).unwrap().word(), *w);
        }
    }
}
