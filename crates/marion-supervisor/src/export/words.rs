//! The phrases both renderers print — a token count, a duration, a usage line, a totals line — in
//! one place, so the Markdown and the HTML report say the same thing in the same words. Tokens
//! only, never a price.

use marion_core::contract::TokenUsage;
use marion_core::proto::result::DiffStat;

use super::model::{CheckLine, NodeReport, Report};

/// A token count as a person reads it: `812`, `12.4k`, `1.3M`.
pub fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// A span of whole seconds: `41s`, `4m19s`, `1h02m`.
pub fn span(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs / 60 % 60),
    }
}

/// A measured command's time: `412ms`, `41.2s`, then [`span`].
pub fn millis(ms: u64) -> String {
    match ms {
        0..1_000 => format!("{ms}ms"),
        1_000..60_000 => format!("{:.1}s", ms as f64 / 1e3),
        _ => span(ms / 1000),
    }
}

/// `42.0k in · 5.1k out · 210.0k cached`, the cache written and the reasoning share only where
/// there were any.
pub fn usage(u: &TokenUsage) -> String {
    let mut parts = vec![
        format!("{} in", tokens(u.input)),
        format!("{} out", tokens(u.output)),
    ];
    if u.cache_read > 0 {
        parts.push(format!("{} cached", tokens(u.cache_read)));
    }
    if u.cache_write > 0 {
        parts.push(format!("{} cache written", tokens(u.cache_write)));
    }
    if let Some(r) = u.reasoning.filter(|r| *r > 0) {
        parts.push(format!("{} reasoning", tokens(r)));
    }
    parts.join(" · ")
}

/// The report's one-line summary: nodes, tokens and how many nodes claimed them, changed files,
/// wall time or how many still run.
pub fn totals(r: &Report) -> String {
    let t = &r.totals;
    let mut parts = vec![match t.nodes {
        1 => "1 node".to_string(),
        n => format!("{n} nodes"),
    }];
    parts.push(if t.claimed == t.nodes {
        format!("{} tokens", tokens(t.tokens))
    } else {
        format!(
            "{} tokens ({} of {} nodes said)",
            tokens(t.tokens),
            t.claimed,
            t.nodes
        )
    });
    if t.changed > 0 {
        parts.push(format!("Σ {} files changed", t.changed));
    }
    match (t.live, t.wall()) {
        (0, Some(w)) => parts.push(format!("{} wall", span(w.as_secs()))),
        (0, None) => {}
        (n, _) => parts.push(format!("{n} still running")),
    }
    parts.join(" · ")
}

/// How long a node ran, when it has ended.
pub fn ran_for(n: &NodeReport) -> Option<String> {
    let d = n.ended?.0.duration_since(n.started?.0).ok()?;
    Some(span(d.as_secs()))
}

/// A check's outcome in a word and a mark: `✓`, `✗ exit 101`, `✗ timed out`, `✗ signalled`.
pub fn check_outcome(c: &CheckLine) -> String {
    match (c.timed_out, c.exit) {
        (true, _) => "✗ timed out".into(),
        (false, Some(0)) => "✓".into(),
        (false, Some(code)) => format!("✗ exit {code}"),
        (false, None) => "✗ signalled".into(),
    }
}

/// `+84 −12 across 3 files`.
pub fn diff_stat(d: &DiffStat) -> String {
    let files = match d.files {
        1 => "1 file".to_string(),
        n => format!("{n} files"),
    };
    format!("+{} −{} across {files}", d.added, d.removed)
}

/// A commit id as a person reads it: its first 12 characters.
pub fn short_commit(c: &str) -> &str {
    c.get(..12).unwrap_or(c)
}

/// An event time as a person reads it: `2026-09-21 16:26:40 UTC`.
pub fn time(t: &marion_core::encoding::SystemTime) -> String {
    let s = crate::activity::rfc3339(*t);
    match (s.get(..10), s.get(11..19)) {
        (Some(d), Some(hms)) => format!("{d} {hms} UTC"),
        _ => s,
    }
}

/// The harness, and the model where one reached it.
pub fn harness(n: &NodeReport) -> String {
    match &n.model {
        Some(m) => format!("{} ({m})", n.harness),
        None => n.harness.clone(),
    }
}

/// What a report says instead of a withheld root prompt.
pub const PROMPT_WITHHELD: &str =
    "(the root's prompt is withheld; export with --include-prompt to include it)";

/// What a report says of a node whose stream held no actions marion could read.
pub const NO_ACTIVITY: &str = "(no activity recorded)";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_times_read_short() {
        assert_eq!(tokens(812), "812");
        assert_eq!(tokens(12_400), "12.4k");
        assert_eq!(tokens(1_300_000), "1.3M");
        assert_eq!(span(41), "41s");
        assert_eq!(span(259), "4m19s");
        assert_eq!(span(3729), "1h02m");
        assert_eq!(millis(412), "412ms");
        assert_eq!(millis(41_200), "41.2s");
        assert_eq!(millis(125_000), "2m05s");
    }

    #[test]
    fn a_check_says_how_it_ended() {
        let c = |exit, timed_out| CheckLine {
            command: "cargo test".into(),
            exit,
            ms: 1,
            timed_out,
            output: None,
        };
        assert_eq!(check_outcome(&c(Some(0), false)), "✓");
        assert_eq!(check_outcome(&c(Some(101), false)), "✗ exit 101");
        assert_eq!(check_outcome(&c(None, false)), "✗ signalled");
        assert_eq!(check_outcome(&c(Some(0), true)), "✗ timed out");
    }
}
