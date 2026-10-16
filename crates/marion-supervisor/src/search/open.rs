//! **The one opener for every file search reads that a node could have written or replaced**: its
//! contract, its `prompt.txt`, its `root-change.json`.
//!
//! A node owns its agent dir, so anything there may be a FIFO, a link to `/dev/zero`, a link to
//! a key under `~/.ssh` or a file of any size. Every read here therefore:
//!
//! - takes ids that passed [`is_uuid_v7`] (a project hash, [`is_project_hash`]) before any path is
//!   joined, so `../../x` read back off a file names nothing;
//! - walks from the project dir with `openat`, every directory `O_DIRECTORY|O_NOFOLLOW`, and opens
//!   the leaf `O_RDONLY|O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC`, so no link is followed at any level and
//!   opening a FIFO cannot hang;
//! - accepts a regular file only, by `fstat` on the descriptor it opened;
//! - reads at most a per-kind cap, and refuses a file longer than it.
//!
//! A refusal is a [`Refusal`], which the caller skips and counts. It never fails a search.
//!
//! [`is_uuid_v7`]: marion_core::ids::is_uuid_v7
//! [`is_project_hash`]: crate::search::digest::is_project_hash

use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{FileType, Mode, OFlags};

use marion_core::ids::is_uuid_v7;

/// A kind of file search reads from an agent dir, and the most of it that is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `contracts/<task>.json`. The caps in `marion_core::cap` bound a real one well below this.
    Contract,
    /// `prompt.txt`.
    Prompt,
    /// `root-change.json`.
    RootChange,
}

impl Kind {
    pub fn cap(self) -> u64 {
        match self {
            Kind::Contract | Kind::RootChange => 1024 * 1024,
            Kind::Prompt => 64 * 1024,
        }
    }
}

/// Why a file was not read. The words are what a search's footer names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// An agent or task id that is not a UUIDv7.
    BadId,
    /// Nothing there. Not counted: a root that kept no prompt is ordinary.
    Missing,
    /// A symlink at some level, or anything else that would not open as asked.
    Unopenable,
    /// A FIFO, a device, a directory: anything but a regular file.
    NotRegular,
    /// Longer than its kind's cap.
    TooLarge,
    /// Read, and not the document it should be.
    Unparsable,
}

impl Refusal {
    pub fn words(&self) -> &'static str {
        match self {
            Refusal::BadId => "an id that is not marion's",
            Refusal::Missing => "missing",
            Refusal::Unopenable => "not openable without following a link",
            Refusal::NotRegular => "not a regular file",
            Refusal::TooLarge => "larger than search reads",
            Refusal::Unparsable => "not readable as marion wrote it",
        }
    }
}

const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

const LEAF: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

/// **`project/agents/<agent>/<rel…>`, read through every rule above.** `rel` is marion's own
/// constant leaf names plus, for a contract, a task id the caller checked with [`task_leaf`].
pub fn read_agent_file(
    project: &Path,
    agent: &str,
    rel: &[&str],
    kind: Kind,
) -> Result<Vec<u8>, Refusal> {
    if !is_uuid_v7(agent) {
        return Err(Refusal::BadId);
    }
    let base = rustix::fs::open(project, DIR, Mode::empty()).map_err(opened)?;
    let mut dir = rustix::fs::openat(&base, "agents", DIR, Mode::empty()).map_err(opened)?;
    dir = rustix::fs::openat(&dir, agent, DIR, Mode::empty()).map_err(opened)?;
    let Some((leaf, parents)) = rel.split_last() else {
        return Err(Refusal::Unopenable);
    };
    for p in parents {
        dir = rustix::fs::openat(&dir, *p, DIR, Mode::empty()).map_err(opened)?;
    }
    let fd = rustix::fs::openat(&dir, *leaf, LEAF, Mode::empty()).map_err(opened)?;
    read_regular(fd, kind.cap())
}

/// `<task>.json` for a contract, or [`Refusal::BadId`] for a task id that is not a UUIDv7.
pub fn task_leaf(task: &str) -> Result<String, Refusal> {
    if is_uuid_v7(task) {
        Ok(format!("{task}.json"))
    } else {
        Err(Refusal::BadId)
    }
}

/// **`path` opened to read, a regular file only, never through a link at its last component**,
/// and without blocking on a FIFO: for the files under `<state>` search reads that are not in an
/// agent dir (a digest, `project.json`), which a process other than marion could still have
/// planted before the sandbox ships.
pub(crate) fn open_regular(path: &Path) -> std::io::Result<File> {
    let fd = rustix::fs::open(path, LEAF, Mode::empty())?;
    let stat = rustix::fs::fstat(&fd)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(std::io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    Ok(File::from(fd))
}

/// At most `cap` bytes of `fd`, which must be a regular file no longer than `cap`.
pub(crate) fn read_regular(fd: OwnedFd, cap: u64) -> Result<Vec<u8>, Refusal> {
    let stat = rustix::fs::fstat(&fd).map_err(|_| Refusal::Unopenable)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(Refusal::NotRegular);
    }
    let mut bytes = Vec::new();
    File::from(fd)
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Refusal::Unopenable)?;
    if bytes.len() as u64 > cap {
        return Err(Refusal::TooLarge);
    }
    Ok(bytes)
}

fn opened(e: rustix::io::Errno) -> Refusal {
    if e == rustix::io::Errno::NOENT {
        Refusal::Missing
    } else {
        Refusal::Unopenable
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use marion_testsupport::scratch;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    const AGENT: &str = "0190f5a0-0000-7000-8000-000000000001";

    /// A FIFO at `p`, as a node could leave one where a file is expected.
    pub(crate) fn mkfifo(p: &Path) {
        let made = std::process::Command::new("mkfifo")
            .arg(p)
            .status()
            .unwrap();
        assert!(made.success(), "mkfifo {}", p.display());
    }

    fn agent_dir(tag: &str) -> (marion_testsupport::Scratch, std::path::PathBuf) {
        let project = scratch(tag);
        let dir = project.join("agents").join(AGENT);
        std::fs::create_dir_all(&dir).unwrap();
        (project, dir)
    }

    #[test]
    fn a_regular_file_under_its_cap_is_read() {
        let (project, dir) = agent_dir("open-regular");
        std::fs::write(dir.join("prompt.txt"), b"the task").unwrap();
        let read = read_agent_file(project.as_ref(), AGENT, &["prompt.txt"], Kind::Prompt);
        assert_eq!(read.unwrap(), b"the task");
    }

    /// A FIFO is refused without hanging: `O_NONBLOCK` opens it at once and `fstat` turns it away.
    /// Mutation: drop `NONBLOCK` and this test blocks for ever in `open`.
    #[test]
    fn a_fifo_is_refused_and_never_hangs() {
        let (project, dir) = agent_dir("open-fifo");
        mkfifo(&dir.join("prompt.txt"));
        let read = read_agent_file(project.as_ref(), AGENT, &["prompt.txt"], Kind::Prompt);
        assert_eq!(read, Err(Refusal::NotRegular));
    }

    #[test]
    fn a_link_to_dev_zero_is_refused() {
        let (project, dir) = agent_dir("open-dev-zero");
        symlink("/dev/zero", dir.join("prompt.txt")).unwrap();
        let read = read_agent_file(project.as_ref(), AGENT, &["prompt.txt"], Kind::Prompt);
        assert_eq!(read, Err(Refusal::Unopenable));
    }

    /// A link to a file outside the state dir is never followed, so its bytes are never read.
    #[test]
    fn a_link_out_of_the_state_dir_is_refused() {
        let (project, dir) = agent_dir("open-link-out");
        let target = scratch("open-link-out-target");
        let secret = target.join("secret");
        std::fs::write(&secret, b"PRIVATE KEY").unwrap();
        symlink(&secret, dir.join("root-change.json")).unwrap();
        let read = read_agent_file(
            project.as_ref(),
            AGENT,
            &["root-change.json"],
            Kind::RootChange,
        );
        assert_eq!(read, Err(Refusal::Unopenable));
    }

    /// The agent dir itself as a link is refused at the directory, not at the leaf.
    #[test]
    fn a_symlinked_agent_dir_is_refused() {
        let project = scratch("open-agent-link");
        let elsewhere = project.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("prompt.txt"), b"not this").unwrap();
        std::fs::create_dir_all(project.join("agents")).unwrap();
        symlink(&elsewhere, project.join("agents").join(AGENT)).unwrap();
        let read = read_agent_file(project.as_ref(), AGENT, &["prompt.txt"], Kind::Prompt);
        assert_eq!(read, Err(Refusal::Unopenable));
    }

    /// An oversized file is refused after reading one byte past its cap, never read whole.
    #[test]
    fn an_oversized_file_is_refused() {
        let (project, dir) = agent_dir("open-oversized");
        let f = File::create(dir.join("prompt.txt")).unwrap();
        f.set_len(Kind::Prompt.cap() * 1024).unwrap();
        let read = read_agent_file(project.as_ref(), AGENT, &["prompt.txt"], Kind::Prompt);
        assert_eq!(read, Err(Refusal::TooLarge));
    }

    #[test]
    fn an_id_that_is_not_a_uuid_is_refused_before_any_path_is_joined() {
        let (project, _) = agent_dir("open-bad-id");
        for bad in ["../../x", "..", AGENT.to_uppercase().as_str(), ""] {
            let read = read_agent_file(project.as_ref(), bad, &["prompt.txt"], Kind::Prompt);
            assert_eq!(read, Err(Refusal::BadId), "{bad}");
        }
        assert_eq!(task_leaf("../x"), Err(Refusal::BadId));
        assert_eq!(task_leaf(AGENT).unwrap(), format!("{AGENT}.json"));
    }

    #[test]
    fn a_missing_file_says_so() {
        let (project, _) = agent_dir("open-missing");
        let read = read_agent_file(project.as_ref(), AGENT, &["prompt.txt"], Kind::Prompt);
        assert_eq!(read, Err(Refusal::Missing));
    }
}
