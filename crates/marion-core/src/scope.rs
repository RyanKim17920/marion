//! Writable-scope resolution and the detective scope check (design §5.4, §6.7).
//!
//! Two lists, both always stored — an omitted one is `["**"]`, never absent — and **a path is
//! writable iff it matches the ceiling *and* the `spawn` request**. The check is a conjunction at
//! match time, not a set operation at spawn time: glob sets have no closed-form intersection, so
//! "compute the intersection" is not implementable as a single glob list.
//!
//! The dialect is pinned because implementations disagree: `globset::Glob` with
//! `literal_separator = true`, so `**` crosses `/` and `*` does not. Two implementations would
//! otherwise produce different `scope_violations` from the same run.

use std::path::Path;

use globset::{GlobBuilder, GlobSetBuilder};

use crate::contract::Glob;

#[derive(Debug, thiserror::Error)]
pub enum ScopeError {
    #[error("bad glob {pattern:?}: {source}")]
    BadGlob {
        pattern: String,
        source: globset::Error,
    },
    /// A `spawn` glob the ceiling could never admit — the parent asked for a scope the agent type
    /// forbids. A spawn-time error, not a silently empty scope.
    #[error("spawn scope {pattern:?} is not admitted by the agent type's ceiling")]
    OutsideCeiling { pattern: String },
}

fn build(globs: &[Glob]) -> Result<globset::GlobSet, ScopeError> {
    let mut b = GlobSetBuilder::new();
    for g in globs {
        // literal_separator(true) is pinned by 5.4: `**` crosses `/`, `*` does not. globset
        // defaults to false, under which `src/*.rs` would match `src/deep/a.rs`.
        let compiled = GlobBuilder::new(&g.0)
            .literal_separator(true)
            .build()
            .map_err(|e| ScopeError::BadGlob {
                pattern: g.0.clone(),
                source: e,
            })?;
        b.add(compiled);
    }
    b.build().map_err(|e| ScopeError::BadGlob {
        pattern: "<set>".into(),
        source: e,
    })
}

/// A resolved scope: the conjunction of an agent type's ceiling and a `spawn` request.
pub struct Scope {
    ceiling: globset::GlobSet,
    requested: globset::GlobSet,
}

impl Scope {
    pub fn new(ceiling: &[Glob], requested: &[Glob]) -> Result<Self, ScopeError> {
        Ok(Self {
            ceiling: build(ceiling)?,
            requested: build(requested)?,
        })
    }

    /// A path is writable iff it matches **both** lists.
    pub fn is_writable(&self, p: &Path) -> bool {
        self.ceiling.is_match(p) && self.requested.is_match(p)
    }

    /// Paths in `changed_paths` failing *either* list. Derived from the full list before any cap
    /// elision, so a cap can never hide a violation.
    pub fn violations<'a, I: IntoIterator<Item = &'a std::path::PathBuf>>(
        &self,
        changed: I,
    ) -> Vec<std::path::PathBuf> {
        changed
            .into_iter()
            .filter(|p| !self.is_writable(p))
            .cloned()
            .collect()
    }
}

/// The spawn-time check: reject a `spawn` glob the ceiling could never admit.
///
/// **Conservative by design.** "Could never admit" is decided from the glob's *literal prefix* —
/// its longest leading run of components with no metacharacter — because the filesystem is never
/// consulted (`src/generated/**` naming a directory the child is meant to *create* is legal, and
/// testing against current contents would reject exactly that normal case). Anything uncertain
/// passes here and is caught by the match-time conjunction; this is an early error, not the
/// enforcement mechanism.
pub fn check_spawn_scope(ceiling: &[Glob], requested: &[Glob]) -> Result<(), ScopeError> {
    let ceil = build(ceiling)?;
    for g in requested {
        let prefix: Vec<&str> =
            g.0.split('/')
                .take_while(|c| !c.contains(['*', '?', '[', '{']))
                .collect();
        if prefix.is_empty() {
            continue; // `**/*.rs` — no literal prefix to test
        }
        let lit = prefix.join("/");
        if !ceil.is_match(&lit) && !ceil.is_match(format!("{lit}/x")) {
            return Err(ScopeError::OutsideCeiling {
                pattern: g.0.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn g(s: &str) -> Glob {
        Glob(s.into())
    }

    #[test]
    fn writable_requires_both_lists() {
        let s = Scope::new(&[g("src/**")], &[g("**")]).unwrap();
        assert!(s.is_writable(Path::new("src/main.rs")));
        // `**` in the spawn request does not widen a narrower ceiling — that is the whole point
        // of the conjunction, and why an omitted list stores as `["**"]` rather than absent.
        assert!(!s.is_writable(Path::new("docs/x.md")));
    }

    #[test]
    fn an_omitted_spawn_scope_yields_the_ceiling() {
        let narrow = Scope::new(&[g("src/**")], &[g("**")]).unwrap();
        let explicit = Scope::new(&[g("src/**")], &[g("src/**")]).unwrap();
        for p in ["src/a.rs", "docs/b.md", "outside/c.txt"] {
            assert_eq!(
                narrow.is_writable(Path::new(p)),
                explicit.is_writable(Path::new(p))
            );
        }
    }

    #[test]
    fn violations_are_paths_failing_either_list() {
        let s = Scope::new(&[g("**")], &[g("src/**")]).unwrap();
        let changed = vec![PathBuf::from("src/a.rs"), PathBuf::from("outside/b.txt")];
        assert_eq!(s.violations(&changed), vec![PathBuf::from("outside/b.txt")]);
    }

    #[test]
    fn literal_separator_is_pinned() {
        // `*` must not cross `/`; if it did, two implementations would derive different
        // violations from the same run.
        let s = Scope::new(&[g("**")], &[g("src/*.rs")]).unwrap();
        assert!(s.is_writable(Path::new("src/a.rs")));
        assert!(!s.is_writable(Path::new("src/deep/a.rs")));
    }

    #[test]
    fn spawn_scope_outside_the_ceiling_is_a_spawn_time_error() {
        let e = check_spawn_scope(&[g("docs/**")], &[g("src/**")]);
        assert!(matches!(e, Err(ScopeError::OutsideCeiling { .. })));
    }

    #[test]
    fn a_scope_naming_a_directory_the_child_will_create_is_legal() {
        // The filesystem is never consulted; rejecting this is the normal case the check must
        // not break.
        assert!(check_spawn_scope(&[g("src/**")], &[g("src/generated/**")]).is_ok());
    }

    #[test]
    fn a_glob_with_no_literal_prefix_always_passes_the_early_check() {
        assert!(check_spawn_scope(&[g("src/**")], &[g("**/*.rs")]).is_ok());
    }
}
