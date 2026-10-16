//! **Rebuilding a digest from the journal**: the one time search pays for a full replay.
//!
//! A rebuilt digest starts with a header line, `{"v":1,"built":"<ts>","root":…}`. A search that
//! finds no header (no file, a file appenders started before any rebuild, a format it does not
//! know) or finds `runs.stale` rebuilds first ([`needs_rebuild`]); `marion search --reindex`
//! forces it.
//!
//! The rebuild holds the digest's lock exclusive, waiting at most [`LOCK_WAIT`] for it. It
//! removes `runs.stale` *before* it reads the journal, so a line an appender drops while the
//! rebuild runs leaves a marker this rebuild does not clear and the next search rebuilds again.
//! It replays the journal, reads each ended run's contract (or a root's `prompt.txt` and
//! `root-change.json`) through [`open`], writes the lines oldest-ended first to a staging file
//! in the same private dir, and renames it into place.
//!
//! [`open`]: crate::search::open

use std::fs::TryLockError;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marion_core::contract::TaskContract;
use marion_core::encoding::SystemTime;
use marion_core::paths::{ProjectDir, project_hash};
use marion_core::registry::ReplayedNode;
use marion_core::root_change::RootChange;
use serde::{Deserialize, Serialize};

use super::Skipped;
use super::digest::{Place, RootFacts, Run, VERSION};
use super::open::{Kind, Refusal, read_agent_file, task_leaf};
use crate::export::scrub::Scrubber;

/// How long a rebuild waits for another to finish before it says one is running.
pub const LOCK_WAIT: Duration = Duration::from_secs(5);
const LOCK_PAUSE: Duration = Duration::from_millis(50);

/// A rebuilt digest's first line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub v: u32,
    pub built: SystemTime,
    /// The project's root (its git common dir), where one is known: the current project's, or one
    /// a contract's `RepoIdentity` names and whose hash is this project's.
    #[serde(default)]
    pub root: Option<PathBuf>,
}

/// What a rebuild did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rebuilt {
    pub runs: usize,
    pub skipped: Skipped,
}

/// The header of `place`'s digest, when its first line is one this build writes.
pub fn header(place: &Place) -> Option<Header> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(place.digest())
        .ok()?;
    let mut first = Vec::new();
    file.take(64 * 1024).read_to_end(&mut first).ok()?;
    let line = first.split(|b| *b == b'\n').next()?;
    serde_json::from_slice::<Header>(line)
        .ok()
        .filter(|h| h.v == VERSION)
}

/// Whether a search must rebuild `place`'s digest before it reads it.
pub fn needs_rebuild(place: &Place) -> bool {
    place.stale().exists() || header(place).is_none()
}

/// **Rebuild `project`'s digest from its journal**, under the exclusive lock. `root` is the
/// project's root when the caller knows it.
pub fn rebuild(project: &ProjectDir, root: Option<&Path>) -> Result<Rebuilt, String> {
    let place = Place::of(project).map_err(|e| e.to_string())?;
    let fail = |e: io::Error| format!("rebuilding {}: {e}", place.digest().display());
    place.ensure().map_err(fail)?;
    let lock = crate::private_fs::open_append_no_follow(&place.lock()).map_err(fail)?;
    lock_exclusive_within(&lock, LOCK_WAIT, &place)?;
    match std::fs::remove_file(place.stale()) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(fail(e)),
        _ => {}
    }
    let replay = crate::journal::read(project).map_err(|e| {
        format!(
            "reading the journal at {}: {e}",
            project.journal().display()
        )
    })?;
    let scrub = Scrubber::from_process([]);
    let mut skipped = Skipped::default();
    let mut root = root.map(Path::to_path_buf);
    let mut runs: Vec<Run> = Vec::new();
    for node in replay.nodes() {
        match run_of(project, node, &mut root) {
            Ok(Some(run)) => runs.push(run),
            Ok(None) => {}
            Err(why) => skipped.note(why.words()),
        }
    }
    // Newest-ended last, which is where a reader starts.
    runs.sort_by_key(|r| (r.ended.map(|t| t.0), r.at.0));
    let header = Header {
        v: VERSION,
        built: super::digest::now(),
        root,
    };
    let mut bytes = serde_json::to_vec(&header).expect("a header always serializes");
    bytes.push(b'\n');
    let count = runs.len();
    for run in runs {
        bytes.extend(run.encode(&scrub));
    }
    crate::private_fs::write_atomic(&place.digest(), &bytes).map_err(fail)?;
    Ok(Rebuilt {
        runs: count,
        skipped,
    })
}

/// [`super::digest`]'s line for `node`, read off its files: a child's from its last contract, an
/// ended root's from its prompt and change record. `Ok(None)` for a run that has not ended, or
/// never started. Learns the project's `root` from a contract when it was not known.
fn run_of(
    project: &ProjectDir,
    node: &ReplayedNode,
    root: &mut Option<PathBuf>,
) -> Result<Option<Run>, Refusal> {
    let id = node.agent_id.0.as_str();
    if !marion_core::ids::is_uuid_v7(id) {
        return Err(Refusal::BadId);
    }
    let agent_type = node.agent_type().unwrap_or_default();
    if let Some(c) = node.contracts.last() {
        let leaf = task_leaf(&c.task_id.0)?;
        let bytes = read_agent_file(project.path(), id, &["contracts", &leaf], Kind::Contract)?;
        let contract: TaskContract =
            serde_json::from_slice(&bytes).map_err(|_| Refusal::Unparsable)?;
        if root.is_none() {
            *root = contract
                .repo
                .git_common_dir
                .clone()
                .filter(|r| is_root_of(project, r));
        }
        return Ok(Some(Run::of_child(
            &contract,
            &node.agent_id,
            node.parent_id().cloned(),
            agent_type,
        )));
    }
    let Some(status) = node.exit_status().filter(|_| node.is_root()) else {
        return Ok(None);
    };
    let optional = |rel: &str, kind: Kind| match read_agent_file(project.path(), id, &[rel], kind) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(Refusal::Missing) => Ok(None),
        Err(why) => Err(why),
    };
    let prompt = optional("prompt.txt", Kind::Prompt)?
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let change: Option<RootChange> = match optional("root-change.json", Kind::RootChange)? {
        Some(bytes) => Some(serde_json::from_slice(&bytes).map_err(|_| Refusal::Unparsable)?),
        None => None,
    };
    let harness = node.recorded_harness().map_or("", |h| h.as_str());
    Ok(Some(Run::of_root(RootFacts {
        agent_id: node.agent_id.clone(),
        agent_type,
        harness,
        model: node.model.clone(),
        status: Some(status),
        started: node.spawned_ts,
        ended: node.state_ts,
        tokens: node.usage.as_ref().map(|u| u.total()),
        prompt: &prompt,
        change: change.as_ref(),
    })))
}

/// Whether `root` is the root `project`'s directory is named for.
pub(crate) fn is_root_of(project: &ProjectDir, root: &Path) -> bool {
    project.path().file_name().and_then(|h| h.to_str()) == Some(project_hash(root).as_str())
}

/// Hold `lock` exclusive, waiting at most `wait`; past it, say a rebuild is running.
fn lock_exclusive_within(
    lock: &std::fs::File,
    wait: Duration,
    place: &Place,
) -> Result<(), String> {
    let deadline = Instant::now() + wait;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(()),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(LOCK_PAUSE)
            }
            Err(TryLockError::WouldBlock) => {
                return Err(format!(
                    "another marion process has held {} for {}s rebuilding this project's search \
                     digest; try again when it finishes",
                    place.lock().display(),
                    wait.as_secs()
                ));
            }
            Err(TryLockError::Error(e)) => {
                return Err(format!("locking {}: {e}", place.lock().display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Journal;
    use crate::search::digest::tests::HASH;
    use marion_core::contract::{AgentId, Capped, ExitStatus, ProcessExit, TaskId};
    use marion_core::harness::Harness;
    use marion_core::journal::{
        ContractPersisted, Exited, RecordKind, SpawnIntent, Spawned, WriterId,
    };
    use marion_testsupport::{Scratch, scratch};

    const CHILD: &str = "019fbf94-0000-7000-8000-000000000001";
    const ROOT: &str = "019fbf94-0000-7000-8000-0000000000aa";

    fn intent(id: &str, parent: Option<&str>, task: Option<&TaskId>) -> RecordKind {
        RecordKind::SpawnIntent(SpawnIntent {
            budget: None,
            review_of: None,
            agent_id: AgentId(id.into()),
            parent_id: parent.map(|p| AgentId(p.into())),
            agent_type: "codex".into(),
            harness: Harness::Codex,
            depth: u32::from(parent.is_some()),
            task_id: task.cloned(),
            timeout_secs: None,
            verification: vec![],
            race: None,
            workflow: None,
        })
    }

    fn ended(id: &str) -> [RecordKind; 2] {
        [
            RecordKind::Spawned(Spawned {
                agent_id: AgentId(id.into()),
                harness_version: "0.9.0".into(),
                model: None,
                pid: Some(1),
                start_id: None,
                provider: None,
                route: None,
                credential: None,
            }),
            RecordKind::Exited(Exited {
                agent_id: AgentId(id.into()),
                status: ExitStatus::Ok,
                exit: ProcessExit {
                    code: Some(0),
                    signal: None,
                    description: "exited 0".into(),
                },
            }),
        ]
    }

    fn journal(project: &ProjectDir, kinds: Vec<RecordKind>) {
        let mut j = Journal::open_path(&project.journal(), WriterId("w-search".into())).unwrap();
        for k in kinds {
            j.append(k).unwrap();
        }
    }

    /// A project with one ended child whose contract is on disk, as the run path leaves it.
    fn child_project(tag: &str) -> (Scratch, ProjectDir, TaskContract) {
        let state = scratch(tag);
        let project = ProjectDir::from_hash(&state, HASH);
        let mut contract = crate::bridge::contract_that_ran(crate::spawn::ChildOutcome {
            exit_code: Some(0),
            ..Default::default()
        });
        contract.completion.as_mut().unwrap().narrative = Some(Capped {
            value: "the limiter now refuses bursts".into(),
            truncated: false,
            original_bytes: 30,
        });
        let path = project
            .agent(&AgentId(CHILD.into()))
            .contract(&contract.task_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&contract).unwrap()).unwrap();
        let task = contract.task_id.clone();
        let mut kinds = vec![
            intent(ROOT, None, None),
            intent(CHILD, Some(ROOT), Some(&task)),
        ];
        kinds.extend(ended(CHILD));
        kinds.push(RecordKind::ContractPersisted(ContractPersisted {
            changed: None,
            review: None,
            agent_id: AgentId(CHILD.into()),
            task_id: task,
            requester: AgentId(ROOT.into()),
            status: Some(ExitStatus::Ok),
        }));
        journal(&project, kinds);
        (state, project, contract)
    }

    fn lines(place: &Place) -> Vec<String> {
        std::fs::read_to_string(place.digest())
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn without_at(line: &str) -> serde_json::Value {
        let mut v: serde_json::Value = serde_json::from_str(line).unwrap();
        v.as_object_mut().unwrap().remove("at");
        v
    }

    /// **A rebuild writes the line the run's end appended**: one function builds both, from the
    /// same contract. Only `at`, the moment of writing, differs.
    #[test]
    fn a_rebuild_equals_the_appended_form() {
        let (_state, project, contract) = child_project("rebuild-equals-append");
        let place = Place::of(&project).unwrap();
        let run = Run::of_child(
            &contract,
            &AgentId(CHILD.into()),
            Some(AgentId(ROOT.into())),
            "codex",
        );
        super::super::digest::append(&project, run, &Scrubber::from_process([]));
        let appended = lines(&place);
        assert_eq!(appended.len(), 1);
        let rebuilt = rebuild(&project, None).unwrap();
        assert_eq!(rebuilt.runs, 1, "the root never ended, so only the child");
        let after = lines(&place);
        assert_eq!(after.len(), 2, "a header and the child: {after:?}");
        assert_eq!(without_at(&after[1]), without_at(&appended[0]));
        let header: Header = serde_json::from_str(&after[0]).unwrap();
        assert_eq!(header.v, VERSION);
        assert_eq!(
            header.root, None,
            "/repo/.git does not hash to this project"
        );
    }

    /// No header, or a stale marker, makes the next search rebuild; a rebuild clears both.
    #[test]
    fn a_missing_header_or_a_stale_marker_asks_for_a_rebuild() {
        let (_state, project, contract) = child_project("rebuild-trigger");
        let place = Place::of(&project).unwrap();
        assert!(needs_rebuild(&place), "no file");
        let run = Run::of_child(&contract, &AgentId(CHILD.into()), None, "codex");
        super::super::digest::append(&project, run, &Scrubber::from_process([]));
        assert!(
            needs_rebuild(&place),
            "appenders started the file: no header"
        );
        rebuild(&project, None).unwrap();
        assert!(!needs_rebuild(&place));
        crate::private_fs::open_append_no_follow(&place.stale()).unwrap();
        assert!(needs_rebuild(&place), "an appender dropped a line");
        rebuild(&project, None).unwrap();
        assert!(!place.stale().exists());
        assert!(!needs_rebuild(&place));
    }

    /// **An append racing a rebuild is deferred, never lost**: while the lock is held the line
    /// is dropped and the marker left; the rebuild that follows finds the run in the journal.
    #[test]
    fn an_append_racing_a_rebuild_survives_through_the_next_rebuild() {
        let (_state, project, contract) = child_project("rebuild-race");
        let place = Place::of(&project).unwrap();
        rebuild(&project, None).unwrap();
        let held = crate::private_fs::open_append_no_follow(&place.lock()).unwrap();
        held.lock().unwrap();
        let run = Run::of_child(&contract, &AgentId(CHILD.into()), None, "codex");
        super::super::digest::append(&project, run, &Scrubber::from_process([]));
        assert_eq!(lines(&place).len(), 2, "the line was dropped, not written");
        drop(held);
        assert!(needs_rebuild(&place));
        rebuild(&project, None).unwrap();
        assert!(lines(&place)[1].contains(CHILD));
    }

    /// A rebuild already holding the lock is waited for, then named.
    #[test]
    fn a_held_lock_is_waited_for_then_reported() {
        let state = scratch("rebuild-lock-held");
        let place = Place::at(&state, HASH);
        place.ensure().unwrap();
        let held = crate::private_fs::open_append_no_follow(&place.lock()).unwrap();
        held.lock().unwrap();
        let other = crate::private_fs::open_append_no_follow(&place.lock()).unwrap();
        let e = lock_exclusive_within(&other, Duration::from_millis(120), &place).unwrap_err();
        assert!(e.contains("another marion process"), "{e}");
        drop(held);
        lock_exclusive_within(&other, Duration::from_millis(120), &place).unwrap();
    }

    /// A contract replaced by a FIFO, a link to `/dev/zero`, a link out of state or an oversized
    /// file is skipped and counted, never hung on, never read whole, never copied in.
    #[test]
    fn a_hostile_contract_is_skipped_and_counted() {
        type Plant = fn(&Path, &Path);
        let outside = scratch("rebuild-hostile-outside");
        let plants: [(&str, Plant, &str); 4] = [
            (
                "fifo",
                |p, _| crate::search::open::tests::mkfifo(p),
                Refusal::NotRegular.words(),
            ),
            (
                "dev-zero",
                |p, _| std::os::unix::fs::symlink("/dev/zero", p).unwrap(),
                Refusal::Unopenable.words(),
            ),
            (
                "outside",
                |p, out| {
                    let key = out.join("secret-key");
                    std::fs::write(&key, b"SENTINEL PRIVATE KEY").unwrap();
                    std::os::unix::fs::symlink(key, p).unwrap()
                },
                Refusal::Unopenable.words(),
            ),
            (
                "oversized",
                |p, _| {
                    let f = std::fs::File::create(p).unwrap();
                    f.set_len(Kind::Contract.cap() + 1).unwrap();
                },
                Refusal::TooLarge.words(),
            ),
        ];
        for (tag, plant, why) in plants {
            let (_state, project, contract) = child_project(&format!("rebuild-hostile-{tag}"));
            let path = project
                .agent(&AgentId(CHILD.into()))
                .contract(&contract.task_id);
            std::fs::remove_file(&path).unwrap();
            plant(&path, &outside);
            let rebuilt = rebuild(&project, None).unwrap();
            assert_eq!(rebuilt.runs, 0, "{tag}");
            assert_eq!(
                rebuilt.skipped.0.get(why),
                Some(&1),
                "{tag}: {:?}",
                rebuilt.skipped
            );
            let text = std::fs::read_to_string(Place::of(&project).unwrap().digest()).unwrap();
            assert_eq!(text.lines().count(), 1, "{tag}: only the header");
            assert!(!text.contains("SENTINEL"), "{tag}");
        }
    }

    /// An id read back off the journal that is not a UUIDv7 is never joined into a path.
    #[test]
    fn a_forged_id_in_the_journal_is_skipped_and_counted() {
        let state = scratch("rebuild-forged-id");
        let project = ProjectDir::from_hash(&state, HASH);
        let mut kinds = vec![intent("../../x", None, None)];
        kinds.extend(ended("../../x"));
        journal(&project, kinds);
        let rebuilt = rebuild(&project, None).unwrap();
        assert_eq!(rebuilt.runs, 0);
        assert_eq!(rebuilt.skipped.0.get(Refusal::BadId.words()), Some(&1));
    }

    /// An ended root's line is read from its kept prompt, and the header keeps the root it was
    /// handed.
    #[test]
    fn an_ended_root_is_rebuilt_from_its_prompt() {
        let state = scratch("rebuild-root");
        let project = ProjectDir::from_hash(&state, HASH);
        let mut kinds = vec![intent(ROOT, None, None)];
        kinds.extend(ended(ROOT));
        journal(&project, kinds);
        let dir = project.agent(&AgentId(ROOT.into()));
        std::fs::create_dir_all(dir.path()).unwrap();
        crate::node_detail::persist_root_prompt(&dir, "wire the limiter into ci");
        assert_eq!(
            rebuild(&project, Some(Path::new("/r/.git"))).unwrap().runs,
            1
        );
        let place = Place::of(&project).unwrap();
        let all = lines(&place);
        let run: Run = serde_json::from_str(&all[1]).unwrap();
        assert_eq!(run.prompt, "wire the limiter into ci");
        assert_eq!(run.status, Some(ExitStatus::Ok));
        assert_eq!(run.harness, "codex");
        assert!(run.started.is_some() && run.ended.is_some());
        let header: Header = serde_json::from_str(&all[0]).unwrap();
        assert_eq!(header.root.as_deref(), Some(Path::new("/r/.git")));
    }
}
