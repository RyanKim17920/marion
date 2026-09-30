//! **`marion export`**: a report of one delegation tree — what each node was asked, what it did,
//! what it landed and what it spent — written to be shared, as Markdown or as one self-contained
//! HTML file.
//!
//! Read entirely offline: the journal, each node's contract and its `events.jsonl`, exactly the
//! files a supervisor writes. Nothing here dials, starts or waits for a supervisor, so a report can
//! be written for a tree whose supervisor has long gone, on a machine with no harness installed.
//!
//! Every string of a report passes through [`scrub::Scrubber`] once, at one point
//! ([`model::Report::scrub`]), before it is rendered, and the rendered text passes through it again.

pub mod cli;
pub mod collect;
pub mod html;
pub mod md;
pub mod model;
pub mod scrub;
pub mod words;

#[cfg(test)]
mod fixture;

use std::path::Path;

use crate::credentials::CredentialStore;
use marion_core::secret::Secret;
use model::{Format, Report};
use scrub::Scrubber;

/// `report` as the text of `format`: scrubbed at the one scrub point, rendered, and the rendered
/// text cleaned again — the backstop for anything a renderer adds.
pub fn render(report: Report, scrubber: &Scrubber, format: Format) -> Result<String, String> {
    let report = report.scrub(scrubber)?;
    let text = match format {
        Format::Markdown => md::render(&report),
        Format::Html => html::render(&report),
    };
    Ok(scrubber.clean(&text))
}

/// Write `text` to `path` **owner-only**: created `0600`, an existing file narrowed to `0600`
/// before a byte is written, and a symlink at `path` refused rather than followed — a report is
/// scrubbed, but it still describes the operator's work, and sharing it is theirs to decide.
pub fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)?;
    // `mode` applies only to a file this call creates.
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(text.as_bytes())
}

/// The keys behind `credentials` — the ids a report's endpoint nodes ran on — for the scrubber to
/// remove. **Fails closed**: a store that cannot answer stops the export, because a key marion
/// cannot look up is a key it cannot remove. A credential since logged out has no key to find and
/// is skipped: its node's stream was redacted of it as it was recorded.
pub fn keys_behind(
    credentials: &[String],
    store: &dyn CredentialStore,
) -> Result<Vec<Secret>, String> {
    let mut keys = Vec::new();
    for id in credentials {
        match store.get(id) {
            Ok(Some(k)) => keys.push(k),
            Ok(None) => {}
            Err(e) => {
                return Err(format!(
                    "cannot read the key behind `{id}` to remove it from the report: {e}"
                ));
            }
        }
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::collect::collect;
    use super::fixture::{self, SENTINELS};
    use super::model::{ExportOpts, TimelineMode};

    fn opts(include_prompt: bool) -> ExportOpts {
        ExportOpts {
            include_prompt,
            full_diff: true,
            timeline: TimelineMode::All,
        }
    }

    /// **The tree reads off the journal alone**: the root first and its children under it in
    /// journal order, each node's task, steers, timeline, checks and landing, and the totals.
    #[test]
    fn a_report_reads_the_whole_tree_from_the_files_a_supervisor_leaves() {
        let f = fixture::build("export-collect");
        let c = collect(&f.project, &f.repo, "8ea3", &opts(false), fixture::now()).unwrap();
        let r = &c.report;
        assert_eq!(r.target, "claude 8ea3");
        assert_eq!(
            r.tree,
            [
                "claude 8ea3 · exited:ok · claude (claude-opus-5-5) · 257.1k tokens",
                "├── codex 1b2c · exited:ok · codex (gpt-5.5-codex) · 84.4k tokens",
                "└── codex 5d6e openrouter:gpt-5.5 · exited:failed · codex (gpt-5.5) · 912 tokens",
            ]
        );
        let [root, landed, refused] = &r.nodes[..] else {
            panic!("three nodes: {:?}", r.nodes);
        };
        assert!(root.task.is_none() && root.task_withheld, "{root:?}");
        assert_eq!(landed.parent.as_deref(), Some("8ea3"));
        assert_eq!(landed.steers.len(), 1);
        assert_eq!(landed.steers[0].outcome, "delivered via turn");
        assert_eq!(landed.steers[0].at, "+00:34", "timed from the node's start");
        let texts: Vec<&str> = landed
            .timeline
            .head
            .iter()
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(
            texts[0], "$ rg -n limiter src, -n bucket src … ×3",
            "{texts:?}"
        );
        assert_eq!(
            landed.timeline.head[0].at, "+00:22",
            "timed from the node's start"
        );
        assert_eq!(landed.checks.len(), 2);
        assert_eq!(landed.checks[1].exit, Some(101));
        assert!(
            landed.checks[0].output.is_none(),
            "a passing check shows no output"
        );
        assert_eq!(landed.branch.as_deref(), Some("marion/t-1"));
        assert_eq!(landed.changed_paths.len(), 2);
        assert!(landed.full_diff.is_some());
        assert!(
            refused
                .failure
                .as_deref()
                .unwrap()
                .starts_with("login refused: ")
        );
        assert!(refused.timeline.head.is_empty() && refused.timeline.unread.is_none());
        assert_eq!(c.credentials, [fixture::CREDENTIAL]);
        let t = r.totals;
        assert_eq!((t.nodes, t.live, t.claimed, t.changed), (3, 0, 3, 2));
        assert_eq!(t.tokens, 257_100 + 84_400 + 912);
        assert_eq!(
            t.wall(),
            Some(std::time::Duration::from_secs(259)),
            "first spawn to last exit"
        );

        // A child as the target is its own subtree; an id that names nothing is refused by name.
        let sub = collect(&f.project, &f.repo, "1b2c", &opts(false), fixture::now()).unwrap();
        assert_eq!(sub.report.nodes.len(), 1);
        assert!(sub.report.nodes[0].parent.is_none());
        let none = collect(&f.project, &f.repo, "zzzz", &opts(false), fixture::now());
        assert!(none.unwrap_err().contains("no node `zzzz`"));
    }

    /// **A condensed timeline keeps how the node started and how it ended**, and counts what it
    /// left out between them.
    #[test]
    fn a_condensed_timeline_keeps_the_head_and_the_tail() {
        let f = fixture::build("export-condensed");
        let o = ExportOpts {
            timeline: TimelineMode::Condensed(1),
            ..opts(false)
        };
        let c = collect(&f.project, &f.repo, "1b2c", &o, fixture::now()).unwrap();
        let t = &c.report.nodes[0].timeline;
        assert_eq!(t.head.len(), super::model::HEAD_ACTIONS);
        assert_eq!(t.tail.len(), 1);
        assert!(t.elided > 0, "{t:?}");
        assert!(t.tail[0].text.starts_with("Done."), "{t:?}");
    }

    /// **No secret planted anywhere a report reads from survives the one scrub point**, the home
    /// directory reads as `~`, and the root's prompt is in only when asked for.
    #[test]
    fn the_scrubbed_report_carries_no_planted_secret() {
        let f = fixture::build("export-sentinel-report");
        for include_prompt in [false, true] {
            let c = collect(
                &f.project,
                &f.repo,
                "8ea3",
                &opts(include_prompt),
                fixture::now(),
            )
            .unwrap();
            let raw = serde_json::to_string(&c.report).unwrap();
            // Every sentinel really is in what was read, so its absence below is the scrub's doing.
            for (source, planted) in SENTINELS {
                assert!(
                    raw.contains(planted),
                    "{source} was never planted: {planted}"
                );
            }
            let s = fixture::scrubber(&c.credentials);
            let clean = serde_json::to_string(&c.report.scrub(&s).unwrap()).unwrap();
            for (source, planted) in SENTINELS {
                assert!(!clean.contains(planted), "{source} leaked: {planted}");
            }
            assert!(!clean.contains(fixture::HOME), "the home directory leaked");
            assert!(clean.contains("~/code/app"), "the project reads from ~");
            assert_eq!(
                clean.contains(fixture::ROOT_PROMPT),
                include_prompt,
                "the root prompt is in exactly when asked for"
            );
        }
    }

    /// The report rendered as a sharer sees it: the fixture collected, scrubbed and drawn in
    /// `format`, the marion version pinned so a release does not churn the snapshot.
    pub(super) fn rendered(
        f: &fixture::Fixture,
        o: &ExportOpts,
        format: super::model::Format,
    ) -> String {
        let mut c = collect(&f.project, &f.repo, "8ea3", o, fixture::now()).unwrap();
        c.report.version = "0.0.0-test".into();
        super::render(c.report, &fixture::scrubber(&c.credentials), format).unwrap()
    }

    #[test]
    fn the_markdown_report_matches_its_snapshot() {
        let f = fixture::build("export-md-snapshot");
        let md = rendered(&f, &opts(false), super::model::Format::Markdown);
        insta::assert_snapshot!("run_report_md", md);
    }

    /// **No planted secret reaches the rendered Markdown either**, whatever the options: the
    /// scrub point and the backstop pass together.
    #[test]
    fn the_rendered_markdown_carries_no_planted_secret() {
        let f = fixture::build("export-sentinel-md");
        for include_prompt in [false, true] {
            for timeline in [TimelineMode::All, TimelineMode::Condensed(2)] {
                let o = ExportOpts {
                    timeline,
                    ..opts(include_prompt)
                };
                let md = rendered(&f, &o, super::model::Format::Markdown);
                for (source, planted) in SENTINELS {
                    assert!(!md.contains(planted), "{source} leaked: {planted}");
                }
                assert!(!md.contains(fixture::HOME), "the home directory leaked");
                assert!(md.contains("~/code/app"));
                assert_eq!(md.contains(fixture::ROOT_PROMPT), include_prompt);
            }
        }
    }

    #[test]
    fn the_html_report_matches_its_snapshot() {
        let f = fixture::build("export-html-snapshot");
        let html = rendered(&f, &opts(false), super::model::Format::Html);
        insta::assert_snapshot!("run_report_html", html);
    }

    /// **The HTML page carries no planted secret, loads nothing and runs nothing**: no script, no
    /// stylesheet or image fetched, no outside link, and a policy that forbids all of it anyway.
    #[test]
    fn the_html_report_is_scrubbed_and_self_contained() {
        let f = fixture::build("export-sentinel-html");
        for include_prompt in [false, true] {
            let html = rendered(&f, &opts(include_prompt), super::model::Format::Html);
            for (source, planted) in SENTINELS {
                assert!(!html.contains(planted), "{source} leaked: {planted}");
            }
            assert!(!html.contains(fixture::HOME), "the home directory leaked");
            assert!(html.contains("~/code/app"));
            assert_eq!(html.contains(fixture::ROOT_PROMPT), include_prompt);
            let lower = html.to_ascii_lowercase();
            for banned in [
                "<script",
                "<link",
                "<iframe",
                "<img",
                "src=",
                "href=\"http",
                "href=\"//",
                "url(",
                "@import",
                "javascript:",
            ] {
                assert!(
                    !lower.contains(banned),
                    "the page fetches or runs something: {banned}"
                );
            }
            assert!(html.contains(&format!(
                "<meta http-equiv=\"Content-Security-Policy\" content=\"{}\">",
                super::html::CSP
            )));
        }
    }

    /// **Hostile text stays text**: markup in every field a node can fill is escaped in the HTML
    /// and cannot open a tag or leave its block in the Markdown.
    #[test]
    fn markup_a_node_wrote_is_escaped_in_both_formats() {
        let f = fixture::build("export-hostile");
        let mut c = collect(&f.project, &f.repo, "8ea3", &opts(true), fixture::now()).unwrap();
        let evil = "<script>alert(1)</script><img src=x onerror=alert(2)> ``` </pre></details>";
        for n in &mut c.report.nodes {
            n.label = evil.into();
            n.narrative = Some(evil.into());
            n.failure = Some(evil.into());
            n.changed_paths = vec![evil.into()];
            n.full_diff = Some(evil.into());
            for l in n.timeline.head.iter_mut() {
                l.text = evil.into();
            }
            for c in n.checks.iter_mut() {
                c.command = evil.into();
                c.output = Some(evil.into());
            }
        }
        c.report.tree = vec![format!("└── {evil}"); c.report.nodes.len()];
        let s = fixture::scrubber(&c.credentials);
        let html = super::render(c.report.clone(), &s, super::model::Format::Html).unwrap();
        let lower = html.to_ascii_lowercase();
        assert!(
            !lower.contains("<script") && !lower.contains("<img"),
            "{html}"
        );
        assert_eq!(
            lower.matches("</pre>").count(),
            lower.matches("<pre").count()
        );
        assert_eq!(
            lower.matches("</details>").count(),
            lower.matches("<details").count()
        );
        let md = super::render(c.report, &s, super::model::Format::Markdown).unwrap();
        // In Markdown, code shows verbatim; outside code, no `<` may survive unescaped.
        let prose = outside_code(&md)
            .replace("<details><summary>Full diff</summary>", "")
            .replace("</details>", "");
        assert!(!prose.contains('<'), "markup escaped into prose:\n{prose}");
    }

    /// The text of `md` outside fenced blocks and code spans — where a renderer would read markup.
    fn outside_code(md: &str) -> String {
        let mut out = String::new();
        let mut fence: Option<usize> = None;
        for line in md.lines() {
            // A fence may be indented (a check's output sits inside its list item).
            let ticks = line.trim_start().chars().take_while(|c| *c == '`').count();
            match fence {
                Some(n) if ticks >= n && line.trim().trim_start_matches('`').is_empty() => {
                    fence = None
                }
                Some(_) => {}
                None if ticks >= 3 => fence = Some(ticks),
                None => {
                    // Outside a fence: drop code spans, honouring `\` escapes as CommonMark does.
                    let chars: Vec<char> = line.chars().collect();
                    let mut i = 0;
                    while i < chars.len() {
                        match chars[i] {
                            '\\' => {
                                out.extend(chars.get(i..i + 2).unwrap_or(&chars[i..]));
                                i += 2;
                            }
                            '`' => {
                                let run = chars[i..].iter().take_while(|c| **c == '`').count();
                                let body = i + run;
                                let close = (body..chars.len()).find(|&j| {
                                    chars[j..].iter().take_while(|c| **c == '`').count() == run
                                        && chars[j - 1] != '`'
                                });
                                i = close.map_or(chars.len(), |j| j + run);
                            }
                            c => {
                                out.push(c);
                                i += 1;
                            }
                        }
                    }
                    out.push('\n');
                }
            }
        }
        out
    }

    /// **The documented examples are this fixture's report**, byte for byte, under the options a
    /// plain `marion export` uses — so the page a reader is shown is what the command writes.
    /// `MARION_UPDATE_EXAMPLES=1` rewrites them after a deliberate change.
    #[test]
    fn the_documented_examples_are_the_fixtures_report() {
        let f = fixture::build("export-docs-example");
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
        for (name, format) in [
            ("run-report.md", super::model::Format::Markdown),
            ("run-report.html", super::model::Format::Html),
        ] {
            let text = rendered(&f, &ExportOpts::default(), format);
            let path = dir.join(name);
            if std::env::var_os("MARION_UPDATE_EXAMPLES").is_some() {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(&path, &text).unwrap();
            }
            let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
            assert!(
                on_disk == text,
                "docs/examples/{name} is not what the export writes; rerun with \
                 MARION_UPDATE_EXAMPLES=1 and review the diff"
            );
        }
    }

    /// **`marion export -o` writes owner-only and starts nothing**: the file is `0600` (an
    /// existing one narrowed before it is written), a symlink in its place is refused, and the
    /// state directory holds no socket or lock afterwards — no supervisor was started.
    #[test]
    fn the_verb_writes_an_owner_only_file_and_starts_no_supervisor() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture::build("export-cli");
        let state = f.project.path().parent().unwrap().to_path_buf();
        let before = tree_of(&state);
        let out = marion_testsupport::scratch("export-cli-out");
        let file = out.join("report.md");
        std::fs::write(&file, "old").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let args: Vec<String> = ["8ea3", "-o", file.to_str().unwrap()]
            .iter()
            .map(|a| a.to_string())
            .collect();
        let resolve =
            |_: Option<std::path::PathBuf>, _: Option<&str>| Some((f.repo.clone(), state.clone()));
        assert_eq!(
            super::cli::main(&args, resolve),
            std::process::ExitCode::SUCCESS
        );
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the report is owner-only");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("# marion report: claude 8ea3"), "{text}");
        assert_eq!(
            tree_of(&state),
            before,
            "the export wrote nothing under the state dir"
        );

        let link = out.join("link.md");
        std::os::unix::fs::symlink(out.join("elsewhere.md"), &link).unwrap();
        assert!(
            super::write_private(&link, "x").is_err(),
            "a symlink is refused"
        );
        assert!(!out.join("elsewhere.md").exists());
    }

    /// Every path under `dir`, sorted: what a run left there.
    fn tree_of(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p.clone());
                }
                out.push(p);
            }
        }
        out.sort();
        out
    }

    /// **A store that cannot answer stops the export**: a key marion cannot look up is a key it
    /// cannot remove.
    #[test]
    fn a_credential_store_that_fails_stops_the_export() {
        struct Broken;
        impl crate::credentials::CredentialStore for Broken {
            fn get(
                &self,
                _: &str,
            ) -> Result<Option<marion_core::secret::Secret>, crate::credentials::CredentialError>
            {
                Err(crate::credentials::CredentialError::Keychain(
                    "locked".into(),
                ))
            }
            fn put(
                &self,
                _: &str,
                _: &marion_core::secret::Secret,
            ) -> Result<(), crate::credentials::CredentialError> {
                unreachable!()
            }
            fn delete(&self, _: &str) -> Result<bool, crate::credentials::CredentialError> {
                unreachable!()
            }
            fn describe(&self) -> String {
                "broken".into()
            }
        }
        let err = super::keys_behind(&["openrouter:work".into()], &Broken).unwrap_err();
        assert!(
            err.contains("openrouter:work") && err.contains("locked"),
            "{err}"
        );
        assert!(
            super::keys_behind(&[], &Broken).unwrap().is_empty(),
            "no ids asks nothing"
        );
    }
}
