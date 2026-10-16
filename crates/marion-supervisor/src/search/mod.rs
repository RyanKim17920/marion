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

    /// Its repository is no longer where it was: moved or deleted. Its runs are still found.
    pub fn gone(&self) -> bool {
        self.root.as_deref().is_some_and(|r| !r.exists())
    }
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
