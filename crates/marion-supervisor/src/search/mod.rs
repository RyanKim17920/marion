//! **Search across runs**: which run did X, in this project or in every project under `<state>`.
//!
//! Every project keeps one small append-only digest, `<state>/private/<hash>/runs.jsonl`, with one
//! line per run that ended: its metadata, its task, its report, its checks, its findings and the
//! paths it changed ([`digest`]). A search scans that file newest-first. No index, no daemon and
//! nothing resident between searches: idle costs nothing.
//!
//! The digest is a **projection**, never a source of truth. Everything in it is already in the
//! project's journal, contracts, `prompt.txt` and `root-change.json`, masked by the scrubber every
//! report passes through, so it widens nobody's access. It lives under `<state>/private/`, `0700`
//! with `0600` files, which the OS sandbox denies a node outright once it ships.
//!
//! What search never reads: a pane's `pty.cast` (its output echoes what the operator typed), and
//! steer or send text (the journal keeps only its length and digest).

pub mod digest;
pub mod open;
pub mod rebuild;
pub mod scan;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use marion_core::paths::ProjectDir;
use marion_core::search::Query;

pub use scan::{Hit, Matcher};

/// One project a search reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub dir: ProjectDir,
    /// Its root (the git common dir it is keyed on), when known: the current project's, or the one
    /// its digest's header recorded.
    pub root: Option<PathBuf>,
}

impl Project {
    /// The project `key` (a [`crate::socket::project_root`]) names under `state`.
    pub fn current(state: &Path, key: &Path) -> Project {
        Project {
            dir: ProjectDir::new(state, key),
            root: Some(key.to_path_buf()),
        }
    }

    /// The hash its directory is named by.
    pub fn hash(&self) -> &str {
        self.dir
            .path()
            .file_name()
            .and_then(|h| h.to_str())
            .unwrap_or("")
    }

    /// What a row calls it: the repository's directory name (the parent of a `.git` root), else
    /// the hash, since the hash cannot be reversed.
    pub fn name(&self) -> String {
        let named = |p: &Path| p.file_name().and_then(|n| n.to_str()).map(str::to_string);
        self.root
            .as_deref()
            .and_then(|r| match r.file_name().and_then(|n| n.to_str()) {
                Some(".git") => r.parent().and_then(named),
                _ => named(r),
            })
            .unwrap_or_else(|| self.hash().to_string())
    }

    /// Whether `query`'s `project:` filters (a substring of the name) keep it.
    pub fn wanted_by(&self, query: &Query) -> bool {
        let name = self.name().to_lowercase();
        query.project.is_empty() || query.project.iter().any(|w| name.contains(w.as_str()))
    }

    /// Its repository is no longer where it was: moved or deleted. Its runs are still found.
    pub fn gone(&self) -> bool {
        self.root.as_deref().is_some_and(|r| !r.exists())
    }
}

/// **Every project under `state`**: each `<12 lowercase hex>/` directory, never through a
/// symlink, that holds a journal. Siblings that are not projects (`conformance/`,
/// `harness-pins/`, `private/`) fail the name test. A project's root is what its `project.json`
/// names (written by its supervisor), else what its digest's header recorded, kept only when it
/// hashes to the directory; a project with neither shows as its hash until a rebuild learns one.
pub fn every(state: &Path) -> Vec<Project> {
    let Ok(entries) = std::fs::read_dir(state) else {
        return Vec::new();
    };
    let mut out: Vec<Project> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let is_dir = e.file_type().ok()?.is_dir();
            if !digest::is_project_hash(&name) || !is_dir {
                return None;
            }
            let dir = ProjectDir::from_hash(state, &name);
            std::fs::symlink_metadata(dir.journal())
                .ok()
                .filter(|m| m.file_type().is_file())?;
            let root = recorded_root(&dir);
            Some(Project { dir, root })
        })
        .collect();
    out.sort_by(|a, b| a.hash().cmp(b.hash()));
    out
}

/// The root `project.json` or the digest's header names for `dir`, if it hashes to `dir`.
fn recorded_root(dir: &ProjectDir) -> Option<PathBuf> {
    let from_file = open::open_regular(&dir.path().join("project.json"))
        .ok()
        .and_then(|f| open::read_regular(f.into(), 64 * 1024).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|v| v.get("root")?.as_str().map(PathBuf::from));
    let from_header = || {
        digest::Place::of(dir)
            .ok()
            .and_then(|place| rebuild::header(&place))
            .and_then(|h| h.root)
    };
    from_file
        .or_else(from_header)
        .filter(|r| rebuild::is_root_of(dir, r))
}

/// **What each of `projects` is called on screen**: its [`Project::name`], with the start of its
/// hash after a `·` where two share a name, so two checkouts both called `api` stay apart.
pub fn shown_names(projects: &[Project]) -> Vec<String> {
    let names: Vec<String> = projects.iter().map(Project::name).collect();
    names
        .iter()
        .zip(projects)
        .map(|(n, p)| {
            if names.iter().filter(|m| *m == n).count() > 1 && n != p.hash() {
                format!("{n}·{}", &p.hash()[..4])
            } else {
                n.clone()
            }
        })
        .collect()
}

/// How a search runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// The most runs returned, over every project.
    pub limit: usize,
    /// Rebuild every digest read, as `--reindex` asks.
    pub reindex: bool,
}

/// A run that matched, and which of the searched projects it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub project: usize,
    pub hit: Hit,
}

/// What a search found, over every project it read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Newest-ended first, at most [`Options::limit`].
    pub found: Vec<Found>,
    /// Runs looked at.
    pub searched: usize,
    /// Digests rebuilt before they were read.
    pub rebuilt: usize,
    pub skipped: Skipped,
    /// A project that could not be searched, and why. The others still were.
    pub failed: Vec<(usize, String)>,
}

/// **Search `projects` for `query`**: each project's digest rebuilt first where it has no header
/// or was marked stale ([`rebuild::needs_rebuild`]), then scanned newest-first; the hits of every
/// project merged newest-ended first. A project whose digest cannot be read is named in
/// [`Outcome::failed`] and the others are still searched. The root a rebuilt header names is
/// learned into `projects`.
///
/// Reads files only, as the operator's uid: no daemon is dialled and none is started. The one
/// function the CLI and the TUI share.
pub fn find(projects: &mut [Project], query: &Query, opts: Options) -> Outcome {
    let matcher = Matcher::new(query);
    let mut out = Outcome::default();
    for (i, project) in projects.iter_mut().enumerate() {
        match find_in(project, &matcher, opts) {
            Ok((scanned, rebuilt)) => {
                out.searched += scanned.searched;
                out.skipped.merge(scanned.skipped);
                out.rebuilt += usize::from(rebuilt.is_some());
                if let Some(r) = rebuilt {
                    out.skipped.merge(r.skipped);
                }
                out.found.extend(
                    scanned
                        .hits
                        .into_iter()
                        .map(|hit| Found { project: i, hit }),
                );
            }
            Err(why) => out.failed.push((i, why)),
        }
    }
    out.found
        .sort_by_key(|f| std::cmp::Reverse((f.hit.run.ended.map(|t| t.0), f.hit.run.at.0)));
    out.found.truncate(opts.limit);
    out
}

fn find_in(
    project: &mut Project,
    matcher: &Matcher,
    opts: Options,
) -> Result<(scan::Scanned, Option<rebuild::Rebuilt>), String> {
    let place = digest::Place::of(&project.dir).map_err(|e| e.to_string())?;
    let rebuilt = if opts.reindex || rebuild::needs_rebuild(&place) {
        if !project.dir.journal().exists() && !place.digest().exists() {
            // Nothing has run here: no journal to rebuild from, and nothing to find.
            return Ok((scan::Scanned::default(), None));
        }
        Some(rebuild::rebuild(&project.dir, project.root.as_deref())?)
    } else {
        None
    };
    if project.root.is_none() {
        project.root = rebuild::header(&place).and_then(|h| h.root);
    }
    // Its name is known once the digest is (re)built.
    if !project.wanted_by(matcher.query()) {
        return Ok((scan::Scanned::default(), rebuilt));
    }
    let scanned = scan::scan(&place.digest(), matcher, opts.limit)
        .map_err(|e| format!("reading {}: {e}", place.digest().display()))?;
    Ok((scanned, rebuilt))
}

/// **Runs a search could not read, by why**: counted and named in its footer, never a failure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Skipped(pub BTreeMap<&'static str, usize>);

impl Skipped {
    pub fn note(&mut self, why: &'static str) {
        *self.0.entry(why).or_default() += 1;
    }

    pub fn total(&self) -> usize {
        self.0.values().sum()
    }

    pub fn merge(&mut self, other: Skipped) {
        for (why, n) in other.0 {
            *self.0.entry(why).or_default() += n;
        }
    }

    /// `skipped 2 runs: 1 not a regular file, 1 larger than search reads`, or `None`.
    pub fn words(&self) -> Option<String> {
        let total = self.total();
        if total == 0 {
            return None;
        }
        let each: Vec<String> = self.0.iter().map(|(why, n)| format!("{n} {why}")).collect();
        let runs = if total == 1 { "run" } else { "runs" };
        Some(format!("skipped {total} {runs}: {}", each.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::scrub::Scrubber;
    use crate::search::digest::tests::run;
    use marion_core::encoding::SystemTime;
    use marion_testsupport::scratch;

    const NOW: u64 = 1_792_108_800_000;

    /// A project under `state` whose root is `root`, with a journal and a rebuilt digest
    /// holding `runs`.
    fn project_with(state: &Path, root: &Path, runs: Vec<Run>) -> Project {
        let p = Project::current(state, root);
        std::fs::create_dir_all(p.dir.path()).unwrap();
        crate::journal::Journal::open_path(
            &p.dir.journal(),
            marion_core::journal::WriterId("w".into()),
        )
        .unwrap();
        rebuild::rebuild(&p.dir, Some(root)).unwrap();
        for r in runs {
            digest::append(&p.dir, r, &Scrubber::new([], [], None));
        }
        p
    }

    fn ended(id: &str, ms: u64) -> Run {
        let mut r = run(id);
        r.ended = Some(SystemTime::from_unix_millis(ms));
        r
    }

    use digest::Run;

    #[test]
    fn every_project_is_found_and_siblings_are_not() {
        let state = scratch("search-every");
        let acme = state.join("repos").join("acme").join(".git");
        std::fs::create_dir_all(&acme).unwrap();
        let a = project_with(&state, &acme, vec![ended("a1", NOW - 5_000)]);
        let gone = Path::new("/nonexistent-search-every/billing/.git");
        let b = project_with(&state, gone, vec![ended("b1", NOW - 1_000)]);
        for sibling in ["conformance", "harness-pins", "private"] {
            std::fs::create_dir_all(state.join(sibling)).unwrap();
        }
        let elsewhere = scratch("search-every-elsewhere");
        std::fs::write(elsewhere.join("journal.jsonl"), b"").unwrap();
        std::os::unix::fs::symlink(&*elsewhere, state.join("aaaaaaaaaaaa")).unwrap();
        std::fs::create_dir_all(state.join("bbbbbbbbbbbb")).unwrap();

        let mut found = every(&state);
        let mut hashes: Vec<&str> = found.iter().map(Project::hash).collect();
        hashes.sort();
        let mut want = vec![a.hash(), b.hash()];
        want.sort();
        assert_eq!(
            hashes, want,
            "no sibling, no link, no dir without a journal"
        );
        let names: Vec<String> = found.iter().map(Project::name).collect();
        assert!(names.contains(&"acme".to_string()) && names.contains(&"billing".to_string()));
        assert!(found.iter().any(|p| p.name() == "billing" && p.gone()));
        assert!(found.iter().any(|p| p.name() == "acme" && !p.gone()));

        let opts = Options {
            limit: 20,
            reindex: false,
        };
        let all = find(&mut found, &Query::parse("limiter", NOW).unwrap(), opts);
        let ids: Vec<&str> = all
            .found
            .iter()
            .map(|f| f.hit.run.agent_id.0.as_str())
            .collect();
        assert_eq!(ids, ["b1", "a1"], "merged newest-ended first");
        assert_eq!(all.searched, 2);
        let only = find(&mut found, &Query::parse("project:acm", NOW).unwrap(), opts);
        let ids: Vec<&str> = only
            .found
            .iter()
            .map(|f| f.hit.run.agent_id.0.as_str())
            .collect();
        assert_eq!(ids, ["a1"]);
        let one = find(
            &mut found,
            &Query::parse("", NOW).unwrap(),
            Options { limit: 1, ..opts },
        );
        assert_eq!(one.found.len(), 1, "the limit is over every project");
    }

    /// A project nothing has run in yet has nothing to find, and no digest is made for it.
    #[test]
    fn a_project_with_no_journal_is_left_alone() {
        let state = scratch("search-empty");
        let mut projects = vec![Project::current(&state, Path::new("/r/.git"))];
        let opts = Options {
            limit: 20,
            reindex: false,
        };
        let out = find(&mut projects, &Query::parse("x", NOW).unwrap(), opts);
        assert!(out.found.is_empty() && out.failed.is_empty());
        assert!(!state.join("private").exists());
    }

    /// A FIFO planted where the digest goes is never opened to wait on: it has no header, so the
    /// rebuild renames a real digest over it. Mutation: open it blocking and this test hangs.
    #[test]
    fn a_fifo_digest_is_replaced_without_hanging() {
        let state = scratch("search-fifo-digest");
        let root = Path::new("/r/.git");
        let mut projects = vec![project_with(&state, root, vec![])];
        let place = digest::Place::of(&projects[0].dir).unwrap();
        std::fs::remove_file(place.digest()).unwrap();
        open::tests::mkfifo(&place.digest());
        let opts = Options {
            limit: 20,
            reindex: false,
        };
        let out = find(&mut projects, &Query::parse("x", NOW).unwrap(), opts);
        assert!(out.failed.is_empty(), "{out:?}");
        assert_eq!(out.rebuilt, 1);
        assert!(std::fs::symlink_metadata(place.digest()).unwrap().is_file());
    }

    #[test]
    fn two_projects_with_one_name_are_told_apart() {
        let state = Path::new("/s");
        let p = |root: &str| Project::current(state, Path::new(root));
        let names = shown_names(&[p("/a/api/.git"), p("/b/api/.git"), p("/c/web/.git")]);
        assert!(
            names[0].starts_with("api·") && names[1].starts_with("api·"),
            "{names:?}"
        );
        assert_ne!(names[0], names[1]);
        assert_eq!(names[2], "web");
    }

    #[test]
    fn a_footer_names_what_was_skipped() {
        let mut s = Skipped::default();
        assert_eq!(s.words(), None);
        s.note("not a regular file");
        assert_eq!(s.words().unwrap(), "skipped 1 run: 1 not a regular file");
        s.note("larger than search reads");
        assert_eq!(
            s.words().unwrap(),
            "skipped 2 runs: 1 larger than search reads, 1 not a regular file"
        );
    }
}
