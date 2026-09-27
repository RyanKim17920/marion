//! What the battery writes: one trimmed transcript per probe, one `summary.json` per harness, and
//! one matrix (probe × harness) as JSON and markdown.
//!
//! The transcript format is S36's (`tests/fixtures/app-server-0.155.1/README.md`), so a reader of
//! one fixture reads them all: one JSON object per line, `{"t": seconds, "dir": …, "msg": …}`,
//! where `dir` is `c2s` (a frame written to the harness's stdin), `s2c` (a JSON line from its
//! stdout), `raw` (a stdout line that is not JSON), `err` (a stderr line), `prov` (a provider
//! request as the probe's provider saw it, or its answer) and `note` (the probe's own conclusion —
//! read these first).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use serde_json::{Value, json};

/// Strings longer than this are cut in a committed transcript (S36's bound).
const TRIM: usize = 320;

/// A probe's verdict. `Unsupported` always carries the reason, read from the row where the row
/// states it — a probe that cannot run says why in the same place a failure would.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Unsupported(String),
}

impl Status {
    pub fn word(&self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Unsupported(_) => "UNSUPPORTED",
        }
    }
}

/// One probe's result on one harness: the verdict, what the row led the probe to expect, and what
/// the harness actually did.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub probe: &'static str,
    pub status: Status,
    pub expected: String,
    pub observed: String,
}

impl Outcome {
    pub fn unsupported(probe: &'static str, why: impl Into<String>) -> Self {
        let why = why.into();
        Self {
            probe,
            status: Status::Unsupported(why.clone()),
            expected: String::new(),
            observed: why,
        }
    }

    pub fn judged(
        probe: &'static str,
        pass: bool,
        expected: impl Into<String>,
        observed: impl Into<String>,
    ) -> Self {
        Self {
            probe,
            status: if pass { Status::Pass } else { Status::Fail },
            expected: expected.into(),
            observed: observed.into(),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "status": self.status.word(),
            "expected": self.expected,
            "observed": self.observed,
        });
        if let Status::Unsupported(why) = &self.status {
            v["reason"] = json!(why);
        }
        v
    }
}

/// The path spellings a committed transcript must not carry, and what replaces each (REVIEW.md §3:
/// scratch, home and worktree paths are scrubbed).
#[derive(Debug, Clone, Default)]
pub struct Scrub(Vec<(String, &'static str)>);

impl Scrub {
    pub fn new(scratch: &Path) -> Self {
        let mut pairs = vec![(scratch.to_string_lossy().into_owned(), "<SCRATCH>")];
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        if let Some(w) = workspace {
            pairs.push((w.to_string_lossy().into_owned(), "<WORKTREE>"));
        }
        if let Ok(home) = std::env::var("HOME") {
            pairs.push((home, "<HOME>"));
        }
        // A harness's own temp files (gemini's error reports) name the per-user temp dir.
        let tmp = std::env::temp_dir();
        for t in [Some(tmp.clone()), tmp.canonicalize().ok()]
            .into_iter()
            .flatten()
        {
            let t = t.to_string_lossy().trim_end_matches('/').to_string();
            if t.len() > 4 {
                pairs.push((t, "<TMP>"));
            }
        }
        // Longest first, so a scratch path under $HOME is not half-replaced by `<HOME>`.
        pairs.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
        Self(pairs)
    }

    pub fn text(&self, s: &str) -> String {
        let mut out = s.to_string();
        for (from, to) in &self.0 {
            if !from.is_empty() {
                out = out.replace(from.as_str(), to);
            }
        }
        out
    }
}

/// Trim a value for a committed transcript: long strings cut, paths scrubbed.
pub fn trim(v: &Value, scrub: &Scrub) -> Value {
    match v {
        Value::String(s) => {
            let s = scrub.text(s);
            let n = s.chars().count();
            if n > TRIM {
                let head: String = s.chars().take(TRIM).collect();
                Value::String(format!("{head}…[{} more chars]", n - TRIM))
            } else {
                Value::String(s)
            }
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| trim(x, scrub)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (scrub.text(k), trim(x, scrub)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// One probe's transcript. Every line is trimmed and scrubbed as it is written, so nothing
/// untrimmed ever reaches the fixture tree.
#[derive(Debug)]
pub struct Log {
    file: Mutex<File>,
    t0: Instant,
    scrub: Scrub,
}

impl Log {
    pub fn create(path: &Path, scrub: Scrub) -> Self {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("transcript dir");
        }
        Self {
            file: Mutex::new(File::create(path).expect("transcript file")),
            t0: Instant::now(),
            scrub,
        }
    }

    pub fn secs(&self) -> f64 {
        (self.t0.elapsed().as_millis() as f64) / 1000.0
    }

    pub fn w(&self, dir: &str, msg: &Value) {
        let line = json!({"t": self.secs(), "dir": dir, "msg": trim(msg, &self.scrub)});
        let mut f = self.file.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(f, "{line}");
    }

    pub fn scrub(&self) -> &Scrub {
        &self.scrub
    }

    pub fn note(&self, s: impl AsRef<str>) {
        self.w("note", &json!(s.as_ref()));
    }
}

/// One harness's run of the battery.
#[derive(Debug, Clone)]
pub struct TargetResult {
    pub selector: String,
    pub version: String,
    pub dir: String,
    pub outcomes: Vec<Outcome>,
}

impl TargetResult {
    pub fn to_json(&self) -> Value {
        let probes: serde_json::Map<String, Value> = self
            .outcomes
            .iter()
            .map(|o| (o.probe.to_string(), o.to_json()))
            .collect();
        json!({"version": self.version, "fixtures": self.dir, "probes": probes})
    }
}

/// The matrix file's path under the output directory.
pub fn matrix_path(out: &Path) -> PathBuf {
    out.join("matrix.json")
}

/// Merge this run's results into the committed matrix — harnesses not run this time keep their
/// last committed row — and write both renderings.
pub fn write_matrix(out: &Path, results: &[TargetResult], probes: &[&str]) -> Value {
    let path = matrix_path(out);
    let mut harnesses: BTreeMap<String, Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("harnesses").cloned())
        .and_then(|h| serde_json::from_value(h).ok())
        .unwrap_or_default();
    for r in results {
        harnesses.insert(r.selector.clone(), r.to_json());
    }
    let matrix = json!({
        "probes": probes,
        "harnesses": harnesses,
    });
    std::fs::create_dir_all(out).expect("output dir");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&matrix).expect("matrix serialises") + "\n",
    )
    .expect("write matrix.json");
    std::fs::write(out.join("matrix.md"), markdown(&matrix, probes)).expect("write matrix.md");
    matrix
}

fn cell(v: &Value) -> String {
    let status = v["status"].as_str().unwrap_or("-");
    match status {
        "PASS" => "PASS".into(),
        "FAIL" => "**FAIL**".into(),
        "UNSUPPORTED" => "n/a".into(),
        other => other.into(),
    }
}

/// The matrix as a table, then every non-PASS cell's reason, so the table stays narrow and the
/// reason is one scroll away.
fn markdown(matrix: &Value, probes: &[&str]) -> String {
    let mut s = String::from(
        "# Harness conformance matrix\n\n\
         Written by `crates/marion-supervisor/tests/conformance` (`scripts/conformance.sh`). \
         Every run is $0: marion's canned provider on loopback, scratch homes, no login. \
         `n/a` is UNSUPPORTED, with the reason (from the row) below. Per-probe transcripts are \
         in each harness's directory.\n\n",
    );
    s.push_str("| harness | version |");
    for p in probes {
        s.push_str(&format!(" {p} |"));
    }
    s.push_str("\n|---|---|");
    for _ in probes {
        s.push_str("---|");
    }
    s.push('\n');
    let empty = serde_json::Map::new();
    let harnesses = matrix["harnesses"].as_object().unwrap_or(&empty);
    for (name, row) in harnesses {
        s.push_str(&format!(
            "| {name} | {} |",
            row["version"].as_str().unwrap_or("?")
        ));
        for p in probes {
            s.push_str(&format!(" {} |", cell(&row["probes"][*p])));
        }
        s.push('\n');
    }
    s.push_str("\n## Findings per cell\n");
    for (name, row) in harnesses {
        s.push_str(&format!(
            "\n### {name} {}\n\n",
            row["version"].as_str().unwrap_or("?")
        ));
        for p in probes {
            let c = &row["probes"][*p];
            if c.is_null() {
                continue;
            }
            let observed = c["observed"].as_str().unwrap_or("").replace('\n', " ");
            s.push_str(&format!(
                "- **{p}** {}: {observed}\n",
                c["status"].as_str().unwrap_or("-")
            ));
        }
    }
    s
}

/// The (harness, probe) cells whose status moved from PASS in `baseline` to anything else in
/// `now` — what admission refuses — and every other change, which it only reports.
pub fn compare(baseline: &Value, now: &Value) -> (Vec<String>, Vec<String>) {
    let mut regressions = Vec::new();
    let mut changes = Vec::new();
    let empty = serde_json::Map::new();
    let now_h = now["harnesses"].as_object().unwrap_or(&empty);
    for (name, row) in now_h {
        let Some(base_row) = baseline["harnesses"].get(name) else {
            changes.push(format!("{name}: new row"));
            continue;
        };
        let probes = row["probes"].as_object().unwrap_or(&empty);
        for (probe, cell) in probes {
            let was = base_row["probes"][probe]["status"].as_str().unwrap_or("-");
            let is = cell["status"].as_str().unwrap_or("-");
            if was == is {
                continue;
            }
            let line = format!("{name} {probe}: {was} -> {is}");
            if was == "PASS" {
                regressions.push(line);
            } else {
                changes.push(line);
            }
        }
    }
    (regressions, changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pass_that_turns_into_anything_else_is_a_regression_and_other_moves_are_changes() {
        let base = json!({"harnesses": {
            "a": {"probes": {"P-x": {"status": "PASS"}, "P-y": {"status": "FAIL"}}}}});
        let now = json!({"harnesses": {
            "a": {"probes": {"P-x": {"status": "UNSUPPORTED"}, "P-y": {"status": "PASS"}}},
            "b": {"probes": {}}}});
        let (regressions, changes) = compare(&base, &now);
        assert_eq!(regressions, ["a P-x: PASS -> UNSUPPORTED"]);
        assert_eq!(changes, ["a P-y: FAIL -> PASS", "b: new row"]);
    }

    #[test]
    fn a_committed_transcript_carries_no_scratch_path_and_no_long_string() {
        let scrub = Scrub::new(Path::new("/tmp/mn-501/scratch"));
        let v = trim(
            &json!({"cwd": "/tmp/mn-501/scratch/repo", "big": "x".repeat(400)}),
            &scrub,
        );
        assert_eq!(v["cwd"], "<SCRATCH>/repo");
        let big = v["big"].as_str().unwrap();
        assert!(big.ends_with("…[80 more chars]"), "{big}");
    }
}
