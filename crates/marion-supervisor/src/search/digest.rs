//! **The digest**: one JSON line per run that ended, in `<state>/private/<hash>/runs.jsonl`.
//!
//! Written by whoever writes the run's end record, right after it: the child path after
//! `ContractPersisted`, the root path after its `Exited`. It keeps the journal's discipline, so
//! concurrent writers never interleave inside a line: `O_APPEND`, one `write(2)`, every line under
//! [`MAX_LINE_BYTES`] (the journal's own `MAX_RECORD_BYTES`), and a short write closed with a
//! newline so a reader skips the fragment.
//!
//! **Mask first, then trim.** Every text field passes through [`Scrubber::clean`] before any cap
//! is applied, so a cut can never split a key and leave half of it behind. A line that would pass
//! the cap is trimmed in a fixed order (paths, findings, report, prompt, checks) with a count of
//! what went, and is never refused.
//!
//! **An appender never blocks the run that ended.** It takes the digest's lock shared and without
//! waiting, three tries over about 50 ms. A rebuild holds it exclusive; an appender that still
//! cannot get it drops its line and leaves `runs.stale`, which makes the next search rebuild. A
//! dropped line is deferred, never lost, and the run's own end never waits on search.
//!
//! [`Scrubber::clean`]: crate::export::scrub::Scrubber::clean

use std::fs::{File, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use marion_core::contract::{AgentId, ExitStatus, TaskContract, TaskId};
use marion_core::encoding::SystemTime;
use marion_core::paths::{PROJECT_HASH_LEN, ProjectDir};
use marion_core::root_change::{RootChange, RootDelta};
use serde::{Deserialize, Serialize};

use crate::export::scrub::Scrubber;

/// The digest line's format. A file whose header names another is rebuilt.
pub const VERSION: u32 = 1;

/// The longest line, newline included: the journal's cap, for the same `O_APPEND` reason.
pub const MAX_LINE_BYTES: usize = marion_core::journal::MAX_RECORD_BYTES;

/// The most of a task the digest keeps, in bytes: enough to find a run by what it was asked.
pub const PROMPT_CAP: usize = 4 * 1024;

/// The most of a report the digest keeps, in bytes.
pub const REPORT_CAP: usize = 6 * 1024;

/// The most changed paths one line names, as a contract's own cap does.
pub const PATHS_CAP: usize = 100;

/// How long an appender tries for the shared lock before it leaves the line to a rebuild.
const LOCK_TRIES: u32 = 3;
const LOCK_PAUSE: Duration = Duration::from_millis(25);

/// **Where a project's digest lives**: `<state>/private/<hash>/`, beside the locks and keys the
/// sandbox denies a node, never in `<state>/<hash>/` where a node's own files are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    dir: PathBuf,
}

impl Place {
    /// The digest of `project`, whose last component must be a project hash: 12 lowercase hex.
    pub fn of(project: &ProjectDir) -> io::Result<Place> {
        let path = project.path();
        let hash = path.file_name().and_then(|h| h.to_str()).unwrap_or("");
        let state = path.parent().filter(|_| is_project_hash(hash));
        match state {
            Some(state) => Ok(Place::at(state, hash)),
            None => Err(io::Error::other(format!(
                "{} is not a project's state directory",
                path.display()
            ))),
        }
    }

    /// The digest of the project `hash` names under `state`. The caller has checked the hash.
    pub(crate) fn at(state: &Path, hash: &str) -> Place {
        Place {
            dir: state.join("private").join(hash),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn digest(&self) -> PathBuf {
        self.dir.join("runs.jsonl")
    }

    pub fn lock(&self) -> PathBuf {
        self.dir.join("runs.lock")
    }

    /// Left by an appender that could not take the lock: the next search rebuilds.
    pub fn stale(&self) -> PathBuf {
        self.dir.join("runs.stale")
    }

    /// Create `private/` and `private/<hash>/` `0700` where missing, and refuse either when it is
    /// a symlink: a link planted there would point every file below it somewhere else.
    pub(crate) fn ensure(&self) -> io::Result<()> {
        crate::private_fs::create_dir_all(&self.dir)?;
        for dir in [self.dir.parent().unwrap_or(&self.dir), self.dir.as_path()] {
            if !std::fs::symlink_metadata(dir)?.file_type().is_dir() {
                return Err(io::Error::other(format!(
                    "{} is not a directory (a symlink is refused)",
                    dir.display()
                )));
            }
        }
        Ok(())
    }
}

/// `<project-hash>`'s shape: exactly [`PROJECT_HASH_LEN`] lowercase hex characters. Checked
/// before any path is joined from a name read off the disk.
pub fn is_project_hash(name: &str) -> bool {
    name.len() == PROJECT_HASH_LEN && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// One verification command and whether it passed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub cmd: String,
    pub ok: bool,
}

/// **One run as the digest keeps it.** Everything a person or an agent wrote (`prompt`, `report`,
/// `checks`, `findings`, `paths`, `branch`) is masked before it is written; the rest is marion's
/// own record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub v: u32,
    /// When this line was written. Breaks a tie between two lines with the same `ended`.
    pub at: SystemTime,
    pub agent_id: AgentId,
    #[serde(default)]
    pub parent_id: Option<AgentId>,
    #[serde(default)]
    pub task_id: Option<TaskId>,
    pub agent_type: String,
    pub harness: String,
    #[serde(default)]
    pub model: Option<String>,
    /// How it ended. `None` only for a run whose end nothing recorded.
    #[serde(default)]
    pub status: Option<ExitStatus>,
    /// It ended `ok` with a commit marion landed.
    #[serde(default)]
    pub landed: bool,
    #[serde(default)]
    pub started: Option<SystemTime>,
    #[serde(default)]
    pub ended: Option<SystemTime>,
    #[serde(default)]
    pub tokens: Option<u64>,
    #[serde(default)]
    pub prompt: String,
    /// Bytes of the task this line does not keep.
    #[serde(default)]
    pub prompt_omitted: usize,
    #[serde(default)]
    pub report: String,
    #[serde(default)]
    pub report_omitted: usize,
    #[serde(default)]
    pub checks: Vec<Check>,
    #[serde(default)]
    pub checks_omitted: usize,
    #[serde(default)]
    pub findings: Vec<String>,
    #[serde(default)]
    pub findings_omitted: usize,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub paths_omitted: usize,
    #[serde(default)]
    pub branch: Option<String>,
}

/// What a root's end knows of it: a root has no contract (§9), so its owner hands these in.
#[derive(Debug, Clone)]
pub struct RootFacts<'a> {
    pub agent_id: AgentId,
    pub agent_type: &'a str,
    pub harness: &'a str,
    pub model: Option<String>,
    pub status: Option<ExitStatus>,
    pub started: Option<SystemTime>,
    pub ended: Option<SystemTime>,
    pub tokens: Option<u64>,
    pub prompt: &'a str,
    pub change: Option<&'a RootChange>,
}

impl Run {
    /// A line with marion's facts and nothing anyone wrote.
    fn bare(agent_id: AgentId, agent_type: &str, harness: &str) -> Run {
        Run {
            v: VERSION,
            at: now(),
            agent_id,
            parent_id: None,
            task_id: None,
            agent_type: agent_type.to_string(),
            harness: harness.to_string(),
            model: None,
            status: None,
            landed: false,
            started: None,
            ended: None,
            tokens: None,
            prompt: String::new(),
            prompt_omitted: 0,
            report: String::new(),
            report_omitted: 0,
            checks: Vec::new(),
            checks_omitted: 0,
            findings: Vec::new(),
            findings_omitted: 0,
            paths: Vec::new(),
            paths_omitted: 0,
            branch: None,
        }
    }

    /// **A child's line, from its contract** — the one the run just persisted, or the one a
    /// rebuild read back, so the appended line and the rebuilt one are the same function's.
    pub fn of_child(
        contract: &TaskContract,
        agent_id: &AgentId,
        parent_id: Option<AgentId>,
        agent_type: &str,
    ) -> Run {
        let mut run = Run::bare(
            agent_id.clone(),
            agent_type,
            contract.child.harness.as_str(),
        );
        run.parent_id = parent_id;
        run.task_id = Some(contract.task_id.clone());
        run.model = contract.child.model.clone();
        run.started = Some(contract.timestamps.spawned);
        run.ended = contract.timestamps.exited;
        run.prompt = crate::node_detail::task_sent(contract).prompt;
        let Some(c) = &contract.completion else {
            return run;
        };
        run.status = Some(c.status);
        run.landed = c.status == ExitStatus::Ok && c.commit.is_some();
        run.tokens = c.usage.as_ref().map(|u| u.total());
        run.report = c
            .narrative
            .as_ref()
            .map(|n| n.value.clone())
            .unwrap_or_default();
        run.checks = c
            .evidence
            .iter()
            .map(|o| Check {
                cmd: crate::bridge::command_line(&o.command),
                ok: o.exit_code == Some(0) && !o.timed_out,
            })
            .collect();
        run.checks_omitted = c.evidence_omitted;
        if let Some(f) = &c.findings {
            run.findings = f.findings.iter().map(|f| f.claim.clone()).collect();
            run.findings_omitted = f.findings_omitted;
        }
        run.paths = c
            .changed_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        run.paths_omitted = c.changed_paths_omitted;
        run.branch = c.branch.clone();
        run
    }

    /// **A root's line**, from what its end knows.
    pub fn of_root(f: RootFacts<'_>) -> Run {
        let mut run = Run::bare(f.agent_id, f.agent_type, f.harness);
        run.model = f.model;
        run.status = f.status;
        run.started = f.started;
        run.ended = f.ended;
        run.tokens = f.tokens;
        run.prompt = f.prompt.to_string();
        if let Some(RootDelta::Observed { changed_paths, .. }) =
            f.change.map(|c| &c.working_tree_delta)
        {
            run.paths = changed_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect();
        }
        run
    }

    /// Every text field anyone wrote, through `scrub`.
    fn masked(mut self, scrub: &Scrubber) -> Run {
        self.prompt = scrub.clean(&self.prompt);
        self.report = scrub.clean(&self.report);
        for c in &mut self.checks {
            c.cmd = scrub.clean(&c.cmd);
        }
        for f in &mut self.findings {
            *f = scrub.clean(f);
        }
        for p in &mut self.paths {
            *p = scrub.clean(p);
        }
        self.branch = self.branch.map(|b| scrub.clean(&b));
        self
    }

    /// **The line as written**: masked by `scrub`, the task and report cut to their caps, then
    /// trimmed to [`MAX_LINE_BYTES`] with its newline.
    pub fn encode(self, scrub: &Scrubber) -> Vec<u8> {
        let mut run = self.masked(scrub);
        run.prompt_omitted += cut(&mut run.prompt, PROMPT_CAP);
        run.report_omitted += cut(&mut run.report, REPORT_CAP);
        if run.paths.len() > PATHS_CAP {
            run.paths_omitted += run.paths.len() - PATHS_CAP;
            run.paths.truncate(PATHS_CAP);
        }
        loop {
            let mut bytes = serde_json::to_vec(&run).expect("a digest line always serializes");
            if bytes.len() < MAX_LINE_BYTES {
                bytes.push(b'\n');
                return bytes;
            }
            let over = bytes.len() + 1 - MAX_LINE_BYTES;
            if run.paths.pop().is_some() {
                run.paths_omitted += 1;
            } else if run.findings.pop().is_some() {
                run.findings_omitted += 1;
            } else if !run.report.is_empty() {
                let keep = run.report.len().saturating_sub(over);
                run.report_omitted += cut(&mut run.report, keep);
            } else if !run.prompt.is_empty() {
                let keep = run.prompt.len().saturating_sub(over);
                run.prompt_omitted += cut(&mut run.prompt, keep);
            } else if run.checks.pop().is_some() {
                run.checks_omitted += 1;
            } else {
                // Only marion's own short fields are left, and they cannot reach the cap; a
                // branch name is the one text field that could, and it goes last.
                run.branch = None;
            }
        }
    }
}

/// Now, to the millisecond the line's encoding keeps, so a line reads back as it was built.
pub(crate) fn now() -> SystemTime {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    SystemTime::from_unix_millis(ms)
}

/// Cut `text` to at most `max` bytes at a character boundary; how many bytes went.
fn cut(text: &mut String, max: usize) -> usize {
    if text.len() <= max {
        return 0;
    }
    let mut at = max;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    let gone = text.len() - at;
    text.truncate(at);
    gone
}

/// **Append `run` to `project`'s digest**, masked by `scrub`. Best-effort by the policy every
/// view of a run follows: a digest that cannot be written is said on stderr and the run's end goes
/// on, and the next `marion search --reindex` (or rebuild) recovers the line from the journal.
pub fn append(project: &ProjectDir, run: Run, scrub: &Scrubber) {
    let line = run.encode(scrub);
    if let Err(e) = Place::of(project).and_then(|place| append_line(&place, &line)) {
        eprintln!(
            "marion: search digest for {}: {e}",
            project.path().display()
        );
    }
}

/// `line` onto `place`'s digest under the shared lock, or `runs.stale` when a rebuild holds it.
pub(crate) fn append_line(place: &Place, line: &[u8]) -> io::Result<()> {
    place.ensure()?;
    let lock = crate::private_fs::open_append_no_follow(&place.lock())?;
    if !lock_shared_briefly(&lock)? {
        crate::private_fs::open_append_no_follow(&place.stale())?;
        return Ok(());
    }
    let mut digest = crate::private_fs::open_append_no_follow(&place.digest())?;
    let written = digest.write(line)?;
    if written < line.len() {
        // Close the torn line so the next writer's line starts on its own.
        let _ = digest.write(b"\n");
        return Err(io::Error::other(format!(
            "a digest line was written short ({written} of {} bytes)",
            line.len()
        )));
    }
    Ok(())
}

/// `true` once `lock` is held shared; `false` when a rebuild still holds it after
/// [`LOCK_TRIES`].
fn lock_shared_briefly(lock: &File) -> io::Result<bool> {
    for attempt in 1..=LOCK_TRIES {
        match lock.try_lock_shared() {
            Ok(()) => return Ok(true),
            Err(TryLockError::WouldBlock) if attempt < LOCK_TRIES => std::thread::sleep(LOCK_PAUSE),
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
    Ok(false)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use marion_testsupport::scratch;
    use std::os::unix::fs::PermissionsExt;

    pub(crate) const HASH: &str = "0123456789ab";

    pub(crate) fn run(id: &str) -> Run {
        let mut r = Run::bare(AgentId(id.into()), "codex", "codex");
        r.status = Some(ExitStatus::Ok);
        r.prompt = "add a token-bucket limiter".into();
        r.report = "the limiter now refuses bursts".into();
        r
    }

    fn decode(line: &[u8]) -> Run {
        assert_eq!(line.last(), Some(&b'\n'));
        serde_json::from_slice(&line[..line.len() - 1]).unwrap()
    }

    fn nobody() -> Scrubber {
        Scrubber::new([], [], None)
    }

    #[test]
    fn a_line_round_trips() {
        let r = run("a");
        assert_eq!(decode(&r.clone().encode(&nobody())), r);
    }

    /// Every text field at its contract's maximum still makes one line under the cap, trimmed
    /// in order, with what went counted.
    #[test]
    fn the_cap_holds_for_max_size_inputs_and_trims_in_order() {
        let mut r = run("a");
        r.prompt = "p".repeat(64 * 1024);
        r.report = "r".repeat(8 * 1024);
        r.findings = vec!["w".repeat(512); 16];
        r.paths = vec!["d/".repeat(256); 100];
        r.checks = vec![
            Check {
                cmd: "k".repeat(512),
                ok: true
            };
            16
        ];
        let line = r.encode(&nobody());
        assert!(line.len() <= MAX_LINE_BYTES, "{}", line.len());
        let back = decode(&line);
        assert_eq!(
            (back.paths.len(), back.paths_omitted),
            (0, 100),
            "paths go first"
        );
        assert_eq!(
            (back.findings.len(), back.findings_omitted),
            (0, 16),
            "then findings"
        );
        assert!(back.report.len() < REPORT_CAP, "then the report");
        assert_eq!(back.report.len() + back.report_omitted, 8 * 1024);
        assert_eq!(
            back.prompt.len(),
            PROMPT_CAP,
            "the task is cut only to its own cap"
        );
        assert_eq!(back.prompt_omitted, 64 * 1024 - PROMPT_CAP);
        assert_eq!(back.checks.len(), 16);
    }

    /// With paths and findings gone, the report is cut before the task.
    #[test]
    fn the_report_is_cut_before_the_task() {
        let mut r = run("a");
        r.prompt = "p".repeat(PROMPT_CAP);
        r.report = "r".repeat(REPORT_CAP);
        r.checks = vec![
            Check {
                cmd: "k".repeat(500),
                ok: false
            };
            16
        ];
        let back = decode(&r.encode(&nobody()));
        assert!(back.report_omitted > 0);
        assert_eq!(back.prompt.len(), PROMPT_CAP);
        assert_eq!(back.prompt_omitted, 0);
    }

    /// A key straddling the report's cap is masked whole: the mask runs on the whole text first.
    /// Mutation: cut before masking and the key's head survives.
    #[test]
    fn a_key_straddling_the_cap_is_fully_masked() {
        let key = "sk-ant-api03-SENTINELabcdefghijklmnopqrstuvwxyz0123456789";
        let mut r = run("a");
        r.report = format!("{} {key}", "r".repeat(REPORT_CAP - 20));
        let line = String::from_utf8(r.encode(&nobody())).unwrap();
        assert!(!line.contains("sk-ant"), "{line}");
        assert!(!line.contains("SENTINEL"));
    }

    /// An endpoint's key marion knows is scrubbed by value, wherever it appears.
    #[test]
    fn an_endpoint_key_in_a_report_is_scrubbed() {
        let key = "plainkey-SENTINEL-7f3a";
        let mut r = run("a");
        r.report = format!("I used {key} to call it");
        r.paths = vec![format!("logs/{key}.txt")];
        let scrub = Scrubber::new([marion_core::secret::Secret::new(key)], [], None);
        let line = String::from_utf8(r.encode(&scrub)).unwrap();
        assert!(!line.contains("SENTINEL"), "{line}");
        assert!(line.contains("I used *** to call it"));
    }

    #[test]
    fn a_cut_lands_on_a_character_boundary() {
        let mut s = "é".repeat(10);
        assert_eq!(cut(&mut s, 5), 16);
        assert_eq!(s, "éé");
    }

    #[test]
    fn a_place_needs_a_project_hash() {
        let state = scratch("digest-place");
        let ok = Place::of(&ProjectDir::from_hash(&state, HASH)).unwrap();
        assert_eq!(
            ok.digest(),
            state.join("private").join(HASH).join("runs.jsonl")
        );
        for bad in ["..", "0123456789AB", "0123456789a", "agents"] {
            assert!(
                Place::of(&ProjectDir::from_hash(&state, bad)).is_err(),
                "{bad}"
            );
        }
    }

    /// Each append is exactly one line, the file `0600` and both directories `0700`.
    #[test]
    fn each_append_is_one_line_in_owner_only_files() {
        let state = scratch("digest-append");
        let project = ProjectDir::from_hash(&state, HASH);
        append(&project, run("a"), &nobody());
        append(&project, run("b"), &nobody());
        let place = Place::of(&project).unwrap();
        let text = std::fs::read_to_string(place.digest()).unwrap();
        let ids: Vec<String> = text
            .lines()
            .map(|l| serde_json::from_str::<Run>(l).unwrap().agent_id.0)
            .collect();
        assert_eq!(ids, ["a", "b"]);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&place.digest()), 0o600);
        assert_eq!(mode(place.dir()), 0o700);
        assert_eq!(mode(place.dir().parent().unwrap()), 0o700);
    }

    /// While a rebuild holds the lock, an append returns at once, writes nothing and leaves the
    /// stale marker. Mutation: wait on the lock and this test hangs.
    #[test]
    fn an_append_during_a_rebuild_drops_its_line_and_marks_the_digest_stale() {
        let state = scratch("digest-stale");
        let place = Place::at(&state, HASH);
        place.ensure().unwrap();
        let held = crate::private_fs::open_append_no_follow(&place.lock()).unwrap();
        held.lock().unwrap();
        append_line(&place, &run("a").encode(&nobody())).unwrap();
        assert!(place.stale().exists());
        assert!(!place.digest().exists());
    }

    /// A symlinked `private/<hash>` is refused, and the link's target is never written.
    #[test]
    fn a_symlinked_digest_dir_is_refused() {
        let state = scratch("digest-symlink");
        let elsewhere = state.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::create_dir_all(state.join("private")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, state.join("private").join(HASH)).unwrap();
        let place = Place::at(&state, HASH);
        assert!(append_line(&place, b"{}\n").is_err());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    }
}
