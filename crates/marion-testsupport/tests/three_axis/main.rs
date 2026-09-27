//! **The three-axis checks: harness generality, efficiency, security.**
//!
//! Every marion change is judged on these three axes before it lands. This suite automates the
//! part of that judgement a syntax tree can answer, over every production source file in the
//! workspace, so a regression is a failing `cargo test` rather than something a reviewer has to
//! notice:
//!
//! - **generality** — no branching on *which* harness outside the row files ([`generality`]);
//! - **efficiency** — no timer-driven polling in shipped code ([`efficiency`]);
//! - **security** — no secret in a debug print, a log line, argv or a world-readable file, and no
//!   login flow in tests ([`security`]).
//!
//! # Allowlists are a ratchet
//!
//! `checks/<axis>.allow` lists `path:item  reason` for findings that are known and accepted — most
//! of them tracked debt with a wave that removes them. A finding not on the list fails the check;
//! so does a list entry that no longer matches anything, so a fix must delete its line and the
//! lists only shrink. Run `scripts/three-axis.sh` for the readable report.

mod efficiency;
mod generality;
mod rules;
mod scan;
mod security;
mod walk;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::OnceLock;

use scan::{Axis, FileFacts, Finding};

const HARNESS_ENUM: &str = "crates/marion-core/src/harness.rs";

fn workspace_findings() -> &'static [Finding] {
    static FINDINGS: OnceLock<Vec<Finding>> = OnceLock::new();
    FINDINGS.get_or_init(|| {
        let root = walk::workspace_root();
        let sources = walk::load_workspace(&root);
        let harness = sources
            .iter()
            .find(|s| s.rel == HARNESS_ENUM)
            .unwrap_or_else(|| panic!("three-axis: {HARNESS_ENUM} not found"));
        let vocab = generality::vocabulary(&harness.ast);
        let mut all = Vec::new();
        for src in &sources {
            let facts = FileFacts {
                row_file: generality::ROW_FILES.contains(&src.rel.as_str()),
                secret_docs: security::writes_secret_docs(&src.ast),
            };
            all.extend(scan::scan(src, &vocab, facts));
        }
        all
    })
}

struct Allowlist {
    path: String,
    entries: BTreeMap<String, String>,
}

fn load_allowlist(root: &Path, axis_file: &str) -> Allowlist {
    let path = format!("checks/{axis_file}");
    let text = std::fs::read_to_string(root.join(&path))
        .unwrap_or_else(|e| panic!("three-axis: read {path}: {e}"));
    Allowlist {
        entries: parse_allowlist(&path, &text),
        path,
    }
}

/// `path:item  reason` per line; `#` starts a comment line. A missing reason or a duplicate key is
/// an error, because an unexplained entry is exactly what the list exists to prevent.
fn parse_allowlist(path: &str, text: &str) -> BTreeMap<String, String> {
    let mut entries = BTreeMap::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, reason) = line
            .split_once(char::is_whitespace)
            .map(|(k, r)| (k, r.trim()))
            .unwrap_or((line, ""));
        assert!(
            key.contains(".rs:") && !reason.is_empty(),
            "{path}:{}: expected `path/to/file.rs:item  one-line reason`, got {raw:?}",
            n + 1
        );
        assert!(
            entries
                .insert(key.to_string(), reason.to_string())
                .is_none(),
            "{path}:{}: duplicate entry {key}",
            n + 1
        );
    }
    entries
}

/// Compare findings with the allowlist; `Err` carries the report the author reads.
fn judge(axis: Axis, findings: &[Finding], allow: &Allowlist, fix: &str) -> Result<usize, String> {
    let mine: Vec<&Finding> = findings.iter().filter(|f| f.axis == axis).collect();
    let keys: BTreeSet<String> = mine.iter().map(|f| f.key()).collect();
    let new: Vec<&&Finding> = mine
        .iter()
        .filter(|f| !allow.entries.contains_key(&f.key()))
        .collect();
    let stale: Vec<&String> = allow
        .entries
        .keys()
        .filter(|k| !keys.contains(*k))
        .collect();
    if new.is_empty() && stale.is_empty() {
        return Ok(mine.len());
    }
    let mut r = format!("\nthree-axis {axis:?} check failed.\n");
    if !new.is_empty() {
        r.push_str(&format!("\nNew findings (not in {}):\n", allow.path));
        for f in &new {
            r.push_str(&format!(
                "  {}:{}  [{}] in `{}`: {}\n      allowlist key: {}\n",
                f.file,
                f.line,
                f.rule,
                f.item,
                f.detail,
                f.key()
            ));
        }
        r.push_str(&format!("\nWhat to do instead: {fix}\n"));
    }
    if !stale.is_empty() {
        r.push_str(&format!(
            "\nStale entries in {} — nothing matches them any more. Delete these lines; the list only shrinks:\n",
            allow.path
        ));
        for k in stale {
            r.push_str(&format!("  {k}\n"));
        }
    }
    Err(r)
}

fn run(axis: Axis, file: &str, fix: &str) {
    let root = walk::workspace_root();
    let allow = load_allowlist(&root, file);
    match judge(axis, workspace_findings(), &allow, fix) {
        Ok(n) => eprintln!(
            "three-axis {axis:?}: ok — {n} finding(s), all covered by {} ({} entries)",
            allow.path,
            allow.entries.len()
        ),
        Err(report) => panic!("{report}"),
    }
}

#[test]
fn generality() {
    run(Axis::Generality, "generality.allow", generality::FIX);
}

#[test]
fn efficiency() {
    run(Axis::Efficiency, "efficiency.allow", efficiency::FIX);
}

#[test]
fn security() {
    run(Axis::Security, "security.allow", security::FIX);
}

/// The walk must actually reach the shipped code: a resolver bug that silently found nothing
/// would pass all three checks.
#[test]
fn the_walk_reaches_production_and_test_code() {
    let root = walk::workspace_root();
    let sources = walk::load_workspace(&root);
    let find = |rel: &str| sources.iter().find(|s| s.rel == rel);
    for prod in [
        "crates/marion-supervisor/src/handler.rs",
        "crates/marion-supervisor/src/bin/marion.rs",
        "crates/marion-harness/src/adapter.rs",
        "crates/marion-core/src/harness.rs",
    ] {
        assert!(
            !find(prod).expect(prod).test_file,
            "{prod} should be production code"
        );
    }
    for test in [
        "crates/marion-supervisor/src/pty/tests.rs",
        "crates/marion-testsupport/src/lib.rs",
        "crates/marion-provider/src/lib.rs",
    ] {
        assert!(
            find(test).expect(test).test_file,
            "{test} should be test code"
        );
    }
    assert!(sources.len() > 150, "only {} files found", sources.len());
}
