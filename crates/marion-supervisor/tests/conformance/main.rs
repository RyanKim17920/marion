//! **The harness conformance battery**: every probe written once, driven by row data, run against
//! every harness row marion has — so a harness gets S36's depth of measurement (the codex
//! app-server kit: a logging driver, a hold/script provider) the day its row lands.
//!
//! ```sh
//! scripts/conformance.sh --harness opencode          # one row
//! scripts/conformance.sh --all                       # every row whose binary is installed
//! MARION_CONFORMANCE=opencode,acp:opencode cargo test -p marion-supervisor --test conformance
//! ```
//!
//! **Opt-in, by name.** `MARION_CONFORMANCE` selects rows (`all`, or a comma list of the matrix's
//! row names: `claude-code`, `codex`, `opencode`, `acp:opencode`, … — or a program, which names
//! every row that runs it: `opencode` is both `opencode` and `acp:opencode`). Unset, the test announces
//! its skip and passes: the battery drives real harnesses for minutes each and belongs to
//! admission (`scripts/admit-harness.sh`) and the nightly canary, not to every `cargo test`.
//!
//! **$0, and nobody's login.** Every model request goes to marion's canned provider on loopback
//! through the probe's own hold ([`provider`]); every launch is marion's canned launch, whose rows
//! relocate the harness's config and credential homes into a scratch tree. No probe starts a login.
//!
//! **Output.** Per row, `<out>/<row>-<version>/` holds one trimmed transcript per probe (S36's
//! format, [`report`]) and `summary.json`; `<out>/matrix.json` and `<out>/matrix.md` are the probe ×
//! harness matrix, merged with the rows not run this time. `<out>` defaults to
//! `tests/fixtures/conformance/` and is `MARION_CONFORMANCE_OUT` when set. With
//! `MARION_CONFORMANCE_BASELINE` naming a committed `matrix.json`, a cell that was PASS there and is
//! not now fails the test (admission's diff); every other change is printed.
//!
//! **Rows from files.** The operator's rows (`~/.config/marion/harnesses/`) and every directory in
//! `MARION_HARNESS_DIRS` (`:`-separated) are loaded first and are rows like any other here. They are
//! not marion's, so they stay out of the committed matrix: each one's transcripts go to
//! `<state>/conformance/<row>-<version>/`, and its admission (`row_file::admits`) to
//! `<state>/conformance/<row>-<version>.json`, which `marion doctor` reads back.

mod driver;
mod hygiene;
mod probes;
mod provider;
mod report;
mod target;
mod tty;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

const SELECT: &str = "MARION_CONFORMANCE";
const OUT: &str = "MARION_CONFORMANCE_OUT";
const BASELINE: &str = "MARION_CONFORMANCE_BASELINE";
const HARNESS_DIRS: &str = "MARION_HARNESS_DIRS";

fn default_out() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/conformance")
}

#[test]
fn battery() {
    let Ok(selection) = std::env::var(SELECT) else {
        // Its own line rather than `announce_skip`, whose wording is the CI no-harness switch's.
        eprintln!(
            "SKIPPED ({SELECT} is unset): the harness conformance battery; set it to `all` or a \
             comma list of rows"
        );
        return;
    };
    let wanted: Vec<String> = selection.split(',').map(|s| s.trim().to_string()).collect();
    let all = wanted.iter().any(|w| w == "all");
    let out = std::env::var(OUT)
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_out());
    let scratch = marion_testsupport::scratch("conformance");
    let state = marion_core::paths::state_dir_from_env(None);
    load_row_files();

    let mut results = Vec::new();
    let mut skipped = Vec::new();
    for (selector, built) in target::all() {
        // A row is named by its matrix name, or by its program — what admission knows it by, and
        // `opencode` names both the `run` row and the ACP agent behind the same binary.
        let program = built.as_ref().ok().and_then(target::Target::program);
        let named = all
            || wanted.contains(&selector)
            || program.as_ref().is_some_and(|p| wanted.contains(p));
        if !named {
            continue;
        }
        let t = match built {
            Ok(t) => t,
            Err(e) => {
                skipped.push(format!("{selector}: {e}"));
                continue;
            }
        };
        if let Some(why) = not_installed(&t) {
            skipped.push(format!("{selector}: {why}"));
            continue;
        }
        // A row from a file is recorded under the operator's state, never in marion's matrix.
        let from_file = !t.agent_type.harness.is_builtin();
        let out = match (&state, from_file) {
            (_, false) => out.clone(),
            (Some(state), true) => state.join("conformance"),
            (None, true) => {
                skipped.push(format!("{selector}: no state directory to record it in"));
                continue;
            }
        };
        let staging = out.join(format!(".staging-{}", selector.replace(':', "-")));
        let _ = std::fs::remove_dir_all(&staging);
        let mut ctx = probes::Ctx {
            t: &t,
            out: staging.clone(),
            scratch: scratch.join(selector.replace(':', "-")),
            report_answered: None,
            stand_in: None,
        };
        let (version, mut outcomes) = probes::run_all(&mut ctx);
        let scrub = report::Scrub::new(&scratch);
        for o in &mut outcomes {
            o.observed = scrub.text(&o.observed);
            o.expected = scrub.text(&o.expected);
            if let report::Status::Unsupported(why) = &mut o.status {
                *why = scrub.text(why);
            }
        }
        let dir = probes::fixture_dir(&out, &selector, &version);
        let partial = outcomes.iter().any(|o| o.status == report::Status::NotRun);
        replace_fixture_dir(&out, &selector, &staging, &dir, partial);
        let result = report::TargetResult {
            selector: selector.clone(),
            version,
            dir: dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            outcomes,
        };
        if from_file {
            record_admission(&out, &result, &t);
        } else {
            results.push((result, dir));
        }
    }
    for s in &skipped {
        eprintln!("conformance: skipped {s}");
    }
    assert!(
        !results.is_empty() || !skipped.is_empty() || admitted_any(),
        "{SELECT}={selection} names no row; rows are {:?}",
        target::all()
            .into_iter()
            .map(|(s, _)| s)
            .collect::<Vec<_>>()
    );
    let baseline: Option<Value> = std::env::var(BASELINE)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok());
    let rows: Vec<report::TargetResult> = results.iter().map(|(r, _)| r.clone()).collect();
    let matrix = report::write_matrix(&out, &rows, probes::PROBES);
    // Each row's summary is its merged matrix row, so a partial run's summary still names every
    // probe's last result.
    for (r, dir) in &results {
        std::fs::write(
            dir.join("summary.json"),
            serde_json::to_string_pretty(
                &json!({"row": r.selector, "result": matrix["harnesses"][&r.selector]}),
            )
            .expect("summary serialises")
                + "\n",
        )
        .expect("write summary.json");
    }
    if let Some(base) = baseline {
        let (regressions, changes) = report::compare(&base, &matrix);
        for c in &changes {
            eprintln!("conformance: changed {c}");
        }
        assert!(
            regressions.is_empty(),
            "conformance regressions against the committed matrix:\n{}",
            regressions.join("\n")
        );
    }
}

/// The operator's row files, then each `MARION_HARNESS_DIRS` directory's, into this process.
fn load_row_files() {
    use marion_harness::row_file;
    let mut refused: Vec<String> = row_file::install_user_rows()
        .refused
        .iter()
        .map(ToString::to_string)
        .collect();
    for dir in std::env::var(HARNESS_DIRS).unwrap_or_default().split(':') {
        if !dir.is_empty() {
            refused.extend(
                row_file::install_dir(Path::new(dir))
                    .refused
                    .iter()
                    .map(ToString::to_string),
            );
        }
    }
    for e in refused {
        eprintln!("conformance: harness row not loaded: {e}");
    }
}

static ADMITTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn admitted_any() -> bool {
    ADMITTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// A row from a file: its probes' verdicts and whether they admit it, beside its transcripts, with
/// the digest of the row's text, so doctor can tell a result for an earlier edit.
fn record_admission(out: &Path, r: &report::TargetResult, t: &target::Target) {
    use marion_harness::row_file;
    let probes: Vec<(String, String)> = r
        .outcomes
        .iter()
        .map(|o| (o.probe.to_string(), o.status.word().to_string()))
        .collect();
    let verdict = row_file::admits(t.spec, &probes);
    let h = t.agent_type.harness;
    let record = json!({
        "row": r.selector,
        "version": r.version,
        "admitted": verdict.is_ok(),
        "why": verdict.as_ref().err().cloned().unwrap_or_default(),
        "probes": probes.iter().map(|(p, w)| (p.clone(), Value::from(w.clone()))).collect::<serde_json::Map<_, _>>(),
        "sha256": row_file::source(h).map(|s| s.sha256),
        "transcripts": r.dir,
    });
    let path = row_file::admission_path(
        out.parent().expect("<state>/conformance"),
        h.as_str(),
        &r.version,
    );
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&record).expect("record serialises") + "\n",
    )
    .expect("write the admission record");
    ADMITTED.store(true, std::sync::atomic::Ordering::Relaxed);
    match verdict {
        Ok(()) => eprintln!(
            "conformance: {} {} admitted ({})",
            r.selector,
            r.version,
            path.display()
        ),
        Err(why) => eprintln!(
            "conformance: {} {} NOT admitted: {} ({})",
            r.selector,
            r.version,
            why.join("; "),
            path.display()
        ),
    }
}

/// Why a row's binary cannot be run here, or `None` when it can. Read off the launch marion would
/// compile, so the program name comes from the row and nowhere else.
fn not_installed(t: &target::Target) -> Option<String> {
    let program = t.program()?;
    let found = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(&program).is_file()))
        .unwrap_or(false);
    (!found).then(|| format!("`{program}` is not on PATH"))
}

/// Move this run's transcripts into `<row>-<version>/`, removing the row's older directories:
/// git keeps the history, and the matrix names one directory per row.
///
/// `partial`: some probes were not run, so a transcript this run did not write is carried over
/// from the row's directory for the same version rather than lost with it.
fn replace_fixture_dir(out: &Path, selector: &str, staging: &Path, dir: &Path, partial: bool) {
    if partial {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let to = staging.join(e.file_name());
            if !to.exists() {
                let _ = std::fs::create_dir_all(staging);
                let _ = std::fs::copy(e.path(), to);
            }
        }
    }
    let prefix = format!("{}-", selector.replace(':', "-"));
    if let Ok(entries) = std::fs::read_dir(out) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            // `<row>-<version>`: the row's prefix followed by a version, never another row's name
            // that merely starts with this one (`acp-opencode-…` is not `acp-…`'s).
            let is_row = name.strip_prefix(&prefix).is_some_and(|rest| {
                rest.chars().next().is_some_and(|c| c.is_ascii_digit()) || rest == "unknown"
            });
            if is_row && e.path() != staging {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(staging).expect("staging dir");
    std::fs::rename(staging, dir).expect("move transcripts into place");
    // A probe refused before its process started wrote nothing; an empty transcript says less
    // than its absence beside the summary's reason.
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.metadata().is_ok_and(|m| m.len() == 0) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}
