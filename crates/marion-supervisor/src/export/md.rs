//! **A report as Markdown**, for a pull request body, an issue, or a README: the summary and the
//! tree first, then one section per node.
//!
//! Pure: a [`Report`] in, text out. Everything a node wrote — a narrative, a tool call, a check's
//! output — is untrusted text, so it goes into a code fence or code span long enough that nothing
//! inside can close it, or is escaped where it runs inline, so it can neither break the layout
//! nor smuggle HTML into a renderer that allows it.

use std::fmt::Write;

use marion_core::proto::result::{ActionKind, ActionLine};

use super::model::{NodeReport, Report};
use super::words;

/// The whole report.
pub fn render(r: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# marion report: {}\n", inline(&r.target));
    let _ = writeln!(
        out,
        "{} · project {} · generated {} by marion {}\n",
        inline(&words::totals(r)),
        code(&r.project),
        words::time(&r.generated_at),
        inline(&r.version)
    );
    out.push_str(&fence(&r.tree.join("\n"), "text"));
    out.push('\n');
    for n in &r.nodes {
        node(&mut out, n);
    }
    out
}

fn node(out: &mut String, n: &NodeReport) {
    let _ = writeln!(out, "## {} · {}\n", inline(&n.label), inline(&n.status));
    let mut facts = vec![
        ("harness", inline(&words::harness(n))),
        ("type", inline(&n.agent_type)),
    ];
    if let Some(p) = &n.parent {
        facts.push(("parent", inline(p)));
    }
    if let Some(s) = &n.started {
        let ran = words::ran_for(n).map_or(String::new(), |d| format!(" · ran {d}"));
        facts.push(("started", format!("{}{ran}", words::time(s))));
    }
    if let Some(u) = &n.usage {
        let turns = match n.turns {
            0 => String::new(),
            1 => " · 1 turn".into(),
            t => format!(" · {t} turns"),
        };
        facts.push(("tokens", format!("{}{turns}", words::usage(u))));
    }
    for (k, v) in facts {
        let _ = writeln!(out, "- **{k}**: {v}");
    }
    out.push('\n');

    if let Some(t) = &n.task {
        out.push_str("**Task**\n\n");
        out.push_str(&quote(&t.prompt));
        for a in &t.acceptance {
            let _ = writeln!(out, "- accept: {}", inline(a));
        }
        for v in &t.verification {
            let _ = writeln!(out, "- verify: {}", code(v));
        }
        if !(t.acceptance.is_empty() && t.verification.is_empty()) {
            out.push('\n');
        }
    } else if n.task_withheld {
        let _ = writeln!(out, "**Task** {}\n", inline(words::PROMPT_WITHHELD));
    }

    if !n.steers.is_empty() {
        out.push_str("**Steers** (length and outcome; marion never keeps the text)\n\n");
        for m in &n.steers {
            let _ = writeln!(
                out,
                "- {} {} · {} bytes · {}",
                code(&m.at),
                inline(&m.from),
                m.len,
                inline(&m.outcome)
            );
        }
        out.push('\n');
    }

    out.push_str("**Activity**\n\n");
    match &n.timeline.unread {
        Some(why) => {
            let _ = writeln!(out, "{}\n", inline(why));
        }
        None if n.timeline.head.is_empty() => {
            let _ = writeln!(out, "{}\n", inline(words::NO_ACTIVITY));
        }
        None => {
            let mut lines: Vec<String> = n.timeline.head.iter().map(action).collect();
            if n.timeline.elided > 0 {
                lines.push(format!("        … {} more …", n.timeline.elided));
            }
            lines.extend(n.timeline.tail.iter().map(action));
            out.push_str(&fence(&lines.join("\n"), "text"));
            out.push('\n');
        }
    }

    if !n.checks.is_empty() {
        out.push_str("**Checks**\n\n");
        for c in &n.checks {
            let _ = writeln!(
                out,
                "- {} {} · {}",
                words::check_outcome(c),
                code(&c.command),
                words::millis(c.ms)
            );
            if let Some(o) = &c.output {
                out.push('\n');
                out.push_str(&indent(&fence(o, "text"), "  "));
            }
        }
        out.push('\n');
    }

    if let Some(r) = &n.review {
        let _ = writeln!(out, "**Review**: {}\n", inline(r));
    }

    if let Some(said) = &n.narrative {
        let who = if n.synthesized {
            "**Result** (written by marion; the node reported none)"
        } else {
            "**Result**"
        };
        let _ = writeln!(out, "{who}\n");
        out.push_str(&quote(said));
    }

    if let Some(b) = &n.branch {
        let mut line = format!("**Landed** on {}", code(b));
        if let Some(c) = &n.commit {
            let _ = write!(line, " at {}", code(words::short_commit(c)));
        }
        if let Some(d) = &n.diff {
            let _ = write!(line, " · {}", words::diff_stat(d));
        }
        let _ = writeln!(
            out,
            "{line} · merge with {}\n",
            code(&format!("git merge --no-ff {b}"))
        );
    }
    if !n.changed_paths.is_empty() {
        let mut paths: Vec<String> = n.changed_paths.iter().map(|p| code(p)).collect();
        if n.changed_omitted > 0 {
            paths.push(format!("and {} more", n.changed_omitted));
        }
        let _ = writeln!(out, "Changed: {}\n", paths.join(", "));
    }
    if let Some(d) = &n.full_diff {
        out.push_str("<details><summary>Full diff</summary>\n\n");
        out.push_str(&fence(d, "diff"));
        out.push_str("\n</details>\n\n");
    }

    if let Some(f) = &n.failure {
        let _ = writeln!(out, "**Why it failed**: {}\n", inline(f));
    }
}

/// One timeline line: its time, then what it did — a call as it is, words the node said quoted.
fn action(l: &ActionLine) -> String {
    match l.kind {
        ActionKind::Call | ActionKind::Files => format!("{:>8}  {}", l.at, l.text),
        ActionKind::Said => format!("{:>8}  “{}”", l.at, l.text),
        // A call's end, under the call it ends: its outcome and how long it took.
        ActionKind::Ended => format!("{:>8}    {}", l.at, l.text),
    }
}

/// The longest run of `c` in `s`.
fn longest_run(s: &str, c: char) -> usize {
    let (mut best, mut cur) = (0, 0);
    for ch in s.chars() {
        cur = if ch == c { cur + 1 } else { 0 };
        best = best.max(cur);
    }
    best
}

/// `s` in a fenced block whose fence is longer than any backtick run inside, so nothing in `s`
/// can close it.
pub fn fence(s: &str, lang: &str) -> String {
    let ticks = "`".repeat(longest_run(s, '`').max(2) + 1);
    format!("{ticks}{lang}\n{}\n{ticks}\n", s.trim_end_matches('\n'))
}

/// `s` as a code span, fenced past any backtick run inside and on one line.
pub fn code(s: &str) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let ticks = "`".repeat(longest_run(&one, '`') + 1);
    let pad = if one.starts_with('`') || one.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{ticks}{pad}{one}{pad}{ticks}")
}

/// `s` as running text on one line: Markdown's punctuation escaped, and `<`, `>` and `&` as
/// entities, so a node's words stay words.
pub fn inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.split_whitespace().collect::<Vec<_>>().join(" ").chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\\' | '`' | '*' | '_' | '[' | ']' | '|' | '#' | '~' | '!' => {
                out.push('\\');
                out.push(ch);
            }
            c => out.push(c),
        }
    }
    out
}

/// `s` as a block quote, each line escaped as [`inline`] escapes it; blank lines kept.
fn quote(s: &str) -> String {
    let mut out = String::new();
    for line in s.trim_end().lines() {
        if line.trim().is_empty() {
            out.push_str(">\n");
        } else {
            let _ = writeln!(out, "> {}", inline(line));
        }
    }
    out.push('\n');
    out
}

fn indent(s: &str, by: &str) -> String {
    s.lines().map(|l| format!("{by}{l}\n")).collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Nothing a node wrote can close the block it is in**, and running text can carry no
    /// markup: a fence outgrows the longest backtick run inside it, and inline text is escaped.
    #[test]
    fn untrusted_text_cannot_escape_its_block_or_carry_markup() {
        let hostile = "a ``` b\n````\nc";
        let f = fence(hostile, "text");
        assert!(
            f.starts_with("`````text\n") && f.ends_with("\n`````\n"),
            "{f}"
        );
        assert_eq!(code("x `y` z"), "``x `y` z``");
        assert_eq!(code("`x`"), "`` `x` ``");
        assert_eq!(
            inline("<img src=x onerror=alert(1)> **b** [l](u) a|b"),
            "&lt;img src=x onerror=alert(1)&gt; \\*\\*b\\*\\* \\[l\\](u) a\\|b"
        );
        assert_eq!(inline("two\nlines"), "two lines");
        assert_eq!(quote("one\n\ntwo"), "> one\n>\n> two\n\n");
    }
}
