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

pub mod collect;
pub mod model;
pub mod scrub;

#[cfg(test)]
mod fixture;

use crate::credentials::CredentialStore;
use marion_core::secret::Secret;

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
                "└── codex 5d6e · exited:failed · codex (gpt-5.5) · 912 tokens",
            ]
        );
        let [root, landed, refused] = &r.nodes[..] else {
            panic!("three nodes: {:?}", r.nodes);
        };
        assert!(root.task.is_none() && root.task_withheld, "{root:?}");
        assert_eq!(landed.parent.as_deref(), Some("8ea3"));
        assert_eq!(landed.steers.len(), 1);
        assert_eq!(landed.steers[0].outcome, "delivered via turn");
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
