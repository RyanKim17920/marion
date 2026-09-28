//! **A report as one self-contained HTML file**: open it from disk, attach it to an issue, send
//! it to someone — it needs nothing else and fetches nothing.
//!
//! Pure: a [`Report`] in, text out. The page carries its style inline and **no script**, and its
//! Content-Security-Policy (`default-src 'none'; style-src 'unsafe-inline'`) forbids every fetch —
//! so even text that somehow arrived as markup could neither run nor call out. It does not arrive
//! as markup: every string from the report is escaped, attribute-safe, on its way in. Sections a
//! reader skims fold with `<details>`, which needs no script.

use std::fmt::Write;

use marion_core::proto::result::{ActionKind, ActionLine};

use super::model::{NodeReport, Report};
use super::words;

/// The page's policy: nothing loads, only the inline style applies.
pub const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'";

const STYLE: &str = "\
:root{--bg:#fbfaf8;--fg:#1d1c1a;--dim:#6b6862;--line:#e4e1dc;--card:#fff;--ok:#1f7a4d;\
--bad:#b3261e;--warn:#946200;--live:#2458b3;--code:#f3f1ed;color-scheme:light}\
@media (prefers-color-scheme:dark){:root{--bg:#161614;--fg:#e8e6e1;--dim:#9a968e;--line:#2e2d2a;\
--card:#1e1e1b;--ok:#5fc48f;--bad:#f2867c;--warn:#e0b44d;--live:#86a9f0;--code:#262622;\
color-scheme:dark}}\
*{box-sizing:border-box}\
body{margin:0;background:var(--bg);color:var(--fg);\
font:15px/1.5 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif}\
main{max-width:62rem;margin:0 auto;padding:2rem 1rem 4rem}\
h1{font-size:1.5rem;margin:0 0 .25rem}h2{font-size:1.1rem;margin:0 0 .5rem}\
.sum{font-size:1.05rem;margin:.25rem 0}.meta{color:var(--dim);margin:0 0 1.25rem}\
pre,code{font:13px/1.45 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}\
pre{background:var(--code);padding:.75rem 1rem;border-radius:6px;overflow-x:auto;margin:.5rem 0}\
code{background:var(--code);padding:.05rem .3rem;border-radius:4px}\
pre.tree a{color:inherit;text-decoration:none}pre.tree a:hover{text-decoration:underline}\
section{background:var(--card);border:1px solid var(--line);border-radius:8px;\
padding:1rem 1.25rem;margin:1rem 0}\
.st{font-size:.8rem;font-weight:600;padding:.1rem .45rem;border-radius:999px;\
border:1px solid currentColor;margin-left:.4rem;vertical-align:middle}\
.ok{color:var(--ok)}.bad{color:var(--bad)}.warn{color:var(--warn)}.live{color:var(--live)}\
dl{display:grid;grid-template-columns:max-content 1fr;gap:.15rem 1rem;margin:.25rem 0 .75rem}\
dt{color:var(--dim)}dd{margin:0}\
details{margin:.5rem 0}summary{cursor:pointer;font-weight:600}\
blockquote{margin:.5rem 0;padding:.25rem 1rem;border-left:3px solid var(--line);white-space:pre-wrap}\
ul{margin:.25rem 0;padding-left:1.25rem}.said{color:var(--dim)}.gap{color:var(--dim)}\
.why{color:var(--bad)}";

/// The whole page.
pub fn render(r: &Report) -> String {
    let mut out = String::new();
    let title = format!("marion report: {}", r.target);
    let _ = write!(
        out,
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta http-equiv=\"Content-Security-Policy\" content=\"{CSP}\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
         <title>{}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n<main>\n",
        esc(&title)
    );
    let _ = writeln!(out, "<h1>{}</h1>", esc(&title));
    let _ = writeln!(out, "<p class=\"sum\">{}</p>", esc(&words::totals(r)));
    let _ = writeln!(
        out,
        "<p class=\"meta\">project <code>{}</code> · generated {} by marion {}</p>",
        esc(&r.project),
        esc(&words::time(&r.generated_at)),
        esc(&r.version)
    );
    // The tree, each row a link to its node's section below. Rows and nodes are one walk.
    out.push_str("<pre class=\"tree\">");
    for (i, line) in r.tree.iter().enumerate() {
        let body = line.trim_start_matches(['│', '├', '└', '─', ' ']);
        let lead = &line[..line.len() - body.len()];
        let _ = writeln!(out, "{}<a href=\"#n{i}\">{}</a>", esc(lead), esc(body));
    }
    out.push_str("</pre>\n");
    for (i, n) in r.nodes.iter().enumerate() {
        node(&mut out, i, n);
    }
    out.push_str("</main>\n</body>\n</html>\n");
    out
}

fn node(out: &mut String, i: usize, n: &NodeReport) {
    let _ = writeln!(
        out,
        "<section id=\"n{i}\">\n<h2>{}<span class=\"st {}\">{}</span></h2>",
        esc(&n.label),
        tone(&n.status),
        esc(&n.status)
    );
    out.push_str("<dl>");
    let mut fact = |k: &str, v: &str| {
        let _ = write!(out, "<dt>{}</dt><dd>{}</dd>", esc(k), esc(v));
    };
    fact("harness", &words::harness(n));
    fact("type", &n.agent_type);
    if let Some(p) = &n.parent {
        fact("parent", p);
    }
    if let Some(s) = &n.started {
        let ran = words::ran_for(n).map_or(String::new(), |d| format!(" · ran {d}"));
        fact("started", &format!("{}{ran}", words::time(s)));
    }
    if let Some(u) = &n.usage {
        let turns = match n.turns {
            0 => String::new(),
            1 => " · 1 turn".into(),
            t => format!(" · {t} turns"),
        };
        fact("tokens", &format!("{}{turns}", words::usage(u)));
    }
    out.push_str("</dl>\n");

    if let Some(t) = &n.task {
        out.push_str("<details open><summary>Task</summary>");
        let _ = write!(out, "<blockquote>{}</blockquote>", esc(t.prompt.trim_end()));
        if !(t.acceptance.is_empty() && t.verification.is_empty()) {
            out.push_str("<ul>");
            for a in &t.acceptance {
                let _ = write!(out, "<li>accept: {}</li>", esc(a));
            }
            for v in &t.verification {
                let _ = write!(out, "<li>verify: <code>{}</code></li>", esc(v));
            }
            out.push_str("</ul>");
        }
        out.push_str("</details>\n");
    } else if n.task_withheld {
        let _ = writeln!(
            out,
            "<p><strong>Task</strong> <span class=\"said\">{}</span></p>",
            esc(words::PROMPT_WITHHELD)
        );
    }

    if !n.steers.is_empty() {
        let _ = write!(
            out,
            "<details><summary>Steers ({})</summary><p class=\"said\">length and outcome; marion \
             never keeps the text</p><ul>",
            n.steers.len()
        );
        for m in &n.steers {
            let _ = write!(
                out,
                "<li><code>{}</code> {} · {} bytes · {}</li>",
                esc(&m.at),
                esc(&m.from),
                m.len,
                esc(&m.outcome)
            );
        }
        out.push_str("</ul></details>\n");
    }

    let t = &n.timeline;
    let count = t.head.len() + t.elided + t.tail.len();
    let _ = write!(out, "<details open><summary>Activity ({count})</summary>");
    match &t.unread {
        Some(why) => {
            let _ = write!(out, "<p class=\"said\">{}</p>", esc(why));
        }
        None if t.head.is_empty() => {
            let _ = write!(out, "<p class=\"said\">{}</p>", esc(words::NO_ACTIVITY));
        }
        None => {
            out.push_str("<pre>");
            for l in &t.head {
                action(out, l);
            }
            if t.elided > 0 {
                let _ = writeln!(
                    out,
                    "<span class=\"gap\">        … {} more …</span>",
                    t.elided
                );
            }
            for l in &t.tail {
                action(out, l);
            }
            out.push_str("</pre>");
        }
    }
    out.push_str("</details>\n");

    if !n.checks.is_empty() {
        out.push_str("<details open><summary>Checks</summary><ul>");
        for c in &n.checks {
            let mark = words::check_outcome(c);
            let class = if mark.starts_with('✓') { "ok" } else { "bad" };
            let _ = write!(
                out,
                "<li><span class=\"{class}\">{}</span> <code>{}</code> · {}",
                esc(&mark),
                esc(&c.command),
                esc(&words::millis(c.ms))
            );
            if let Some(o) = &c.output {
                let _ = write!(out, "<pre>{}</pre>", esc(o));
            }
            out.push_str("</li>");
        }
        out.push_str("</ul></details>\n");
    }

    if let Some(r) = &n.review {
        let _ = writeln!(out, "<p><strong>Review</strong> {}</p>", esc(r));
    }

    if let Some(said) = &n.narrative {
        let who = if n.synthesized {
            "Result <span class=\"said\">(written by marion; the node reported none)</span>"
        } else {
            "Result"
        };
        let _ = writeln!(
            out,
            "<details open><summary>{who}</summary><blockquote>{}</blockquote></details>",
            esc(said.trim_end())
        );
    }

    if let Some(b) = &n.branch {
        let _ = write!(out, "<p><strong>Landed</strong> on <code>{}</code>", esc(b));
        if let Some(c) = &n.commit {
            let _ = write!(out, " at <code>{}</code>", esc(words::short_commit(c)));
        }
        if let Some(d) = &n.diff {
            let _ = write!(out, " · {}", esc(&words::diff_stat(d)));
        }
        let _ = writeln!(
            out,
            " · merge with <code>git merge --no-ff {}</code></p>",
            esc(b)
        );
    }
    if !n.changed_paths.is_empty() {
        let mut paths: Vec<String> = n
            .changed_paths
            .iter()
            .map(|p| format!("<code>{}</code>", esc(p)))
            .collect();
        if n.changed_omitted > 0 {
            paths.push(format!("and {} more", n.changed_omitted));
        }
        let _ = writeln!(out, "<p>Changed: {}</p>", paths.join(", "));
    }
    if let Some(d) = &n.full_diff {
        let _ = writeln!(
            out,
            "<details><summary>Full diff</summary><pre>{}</pre></details>",
            esc(d)
        );
    }

    if let Some(f) = &n.failure {
        let _ = writeln!(
            out,
            "<p class=\"why\"><strong>Why it failed</strong> {}</p>",
            esc(f)
        );
    }
    out.push_str("</section>\n");
}

fn action(out: &mut String, l: &ActionLine) {
    match l.kind {
        ActionKind::Call | ActionKind::Files => {
            let _ = writeln!(out, "{:>8}  {}", esc(&l.at), esc(&l.text));
        }
        // A call's end, under the call it ends: its outcome and how long it took.
        ActionKind::Ended => {
            let _ = writeln!(out, "{:>8}    {}", esc(&l.at), esc(&l.text));
        }
        ActionKind::Said => {
            let _ = writeln!(
                out,
                "{:>8}  <span class=\"said\">“{}”</span>",
                esc(&l.at),
                esc(&l.text)
            );
        }
    }
}

/// The badge colour for a state word: ended well, ended badly, waiting on someone, or running.
fn tone(status: &str) -> &'static str {
    match status {
        "exited:ok" => "ok",
        s if s.starts_with("exited:") || s == "orphaned" || s == "reaped" => "bad",
        s if s.starts_with("blocked") => "warn",
        _ => "live",
    }
}

/// `s` safe in text and in a quoted attribute alike.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_markup_character_is_escaped_for_text_and_attributes() {
        assert_eq!(
            esc(r#"<script>alert("x")</script> & 'y'"#),
            "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt; &amp; &#39;y&#39;"
        );
    }

    #[test]
    fn a_state_word_picks_its_badge() {
        assert_eq!(tone("exited:ok"), "ok");
        assert_eq!(tone("exited:timedout"), "bad");
        assert_eq!(tone("orphaned"), "bad");
        assert_eq!(tone("blocked:permission"), "warn");
        assert_eq!(tone("running"), "live");
    }
}
