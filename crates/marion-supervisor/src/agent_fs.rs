//! **The one way the supervisor writes under a node's agent dir.**
//!
//! A node may write its own agent dir: its config documents, its `tmp/`, its worktree, and on
//! Linux (where Landlock grants whole directories) its `contracts/` and `events.jsonl` too. So
//! anything at a path below it may be a link the node left there, to an operator file or
//! directory, and marion's process is the operator's and runs outside every sandbox. A write by
//! path follows such a link; a check by path and then a write by path leaves a gap a process the
//! node double-forked can swap a link into.
//!
//! [`AgentFs`] closes both. The agent dir is opened once, `O_NOFOLLOW`, below its parent (the
//! agents dir, which no node may write). Every directory below it is opened relative to the one
//! above, `O_DIRECTORY | O_NOFOLLOW`, and made `0700` there where missing, so a link or a file in
//! place of any of them refuses the write by name. Files are opened from that directory's fd
//! `O_NOFOLLOW | O_NONBLOCK` and taken only when regular; a document is replaced through a
//! staging file created `O_EXCL | O_NOFOLLOW`, chmodded through its fd and renamed within the same
//! directory fd, so a link at the document is replaced rather than written through. Nothing under
//! the agent dir is opened, chmodded or created by path.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

/// How every directory below the agent dir is opened.
const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// One node's agent dir, open: see the module doc.
#[derive(Debug)]
pub(crate) struct AgentFs {
    root: PathBuf,
    fd: OwnedFd,
}

impl AgentFs {
    /// Open `agent_dir`, making it `0700` where missing: its parent by path (marion's own, made
    /// owner-only where missing), the agent dir itself `O_NOFOLLOW` below that.
    pub(crate) fn open(agent_dir: &Path) -> io::Result<Self> {
        let (Some(parent), Some(name)) = (agent_dir.parent(), agent_dir.file_name()) else {
            return Err(io::Error::other(format!(
                "{} is not a directory below another",
                agent_dir.display()
            )));
        };
        crate::private_fs::create_dir_all(parent)?;
        let above = rustix::fs::open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self {
            fd: subdir(&above, name, agent_dir)?,
            root: agent_dir.to_path_buf(),
        })
    }

    /// The directory at `path` (the agent dir or one below it), each level opened from the one
    /// above and made `0700` where missing, as the module doc says.
    pub(crate) fn dir(&self, path: &Path) -> io::Result<OwnedFd> {
        let mut fd = self.fd.try_clone()?;
        let mut at = self.root.clone();
        for part in self.parts(path)? {
            at.push(part);
            fd = subdir(&fd, part, &at)?;
        }
        Ok(fd)
    }

    /// Replace the document at `path` with `bytes`, `0600`, as the module doc says.
    pub(crate) fn replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let (dir, name) = self.parent_of(path)?;
        crate::private_fs::replace_in(&dir, name, bytes, crate::private_fs::Perm::Private)
    }

    /// Open the file at `path` for appending, created `0600` where missing.
    pub(crate) fn append(&self, path: &Path) -> io::Result<File> {
        self.open_file(path, OFlags::WRONLY | OFlags::APPEND | OFlags::CREATE)
    }

    /// Create or truncate the file at `path` for writing, `0600`.
    pub(crate) fn create(&self, path: &Path) -> io::Result<File> {
        self.open_file(path, OFlags::WRONLY | OFlags::TRUNC | OFlags::CREATE)
    }

    /// Open the file at `path` for reading. A missing file is `NotFound`; anything not a regular
    /// file is refused.
    pub(crate) fn read(&self, path: &Path) -> io::Result<File> {
        let (dir, name) = self.parent_of(path)?;
        let fd = rustix::fs::openat(
            &dir,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        regular(File::from(fd), path)
    }

    /// Rename the entry at `from` to `to`, both below the agent dir, through their directories'
    /// fds: rename(2) moves a link as itself and follows neither name.
    pub(crate) fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (from_dir, from_name) = self.parent_of(from)?;
        let (to_dir, to_name) = self.parent_of(to)?;
        Ok(rustix::fs::renameat(
            &from_dir, from_name, &to_dir, to_name,
        )?)
    }

    /// **A file of the node's own, never a second name for another**: a hard link the node made
    /// to an operator file is a regular file with more than one link, and truncating, appending
    /// or chmodding it would reach that file. So it is opened without `O_TRUNC`, refused when
    /// another name links it, and only then truncated where asked.
    fn open_file(&self, path: &Path, flags: OFlags) -> io::Result<File> {
        use std::os::unix::fs::MetadataExt;
        let (dir, name) = self.parent_of(path)?;
        let fd = rustix::fs::openat(
            &dir,
            name,
            flags.difference(OFlags::TRUNC) | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        let file = regular(File::from(fd), path)?;
        if file.metadata()?.nlink() != 1 {
            return Err(io::Error::other(format!(
                "{} has another name linking it, not a file of the node's own",
                path.display()
            )));
        }
        if flags.contains(OFlags::TRUNC) {
            file.set_len(0)?;
        }
        // `mode` applies only to a file this call created: one that predates the rule is
        // narrowed through the handle.
        rustix::fs::fchmod(&file, Mode::from_raw_mode(0o600))?;
        Ok(file)
    }

    /// The open directory holding `path`, and `path`'s own name in it.
    fn parent_of<'p>(&self, path: &'p Path) -> io::Result<(OwnedFd, &'p OsStr)> {
        let parts = self.parts(path)?;
        let Some((name, dirs)) = parts.split_last() else {
            return Err(self.outside(path));
        };
        let mut fd = self.fd.try_clone()?;
        let mut at = self.root.clone();
        for part in dirs {
            at.push(part);
            fd = subdir(&fd, part, &at)?;
        }
        Ok((fd, name))
    }

    /// `path`'s components below the agent dir, each a plain name.
    fn parts<'p>(&self, path: &'p Path) -> io::Result<Vec<&'p OsStr>> {
        path.strip_prefix(&self.root)
            .map_err(|_| self.outside(path))?
            .components()
            .map(|c| match c {
                Component::Normal(part) => Ok(part),
                _ => Err(self.outside(path)),
            })
            .collect()
    }

    fn outside(&self, path: &Path) -> io::Error {
        io::Error::other(format!(
            "{} is not below the agent dir {}",
            path.display(),
            self.root.display()
        ))
    }
}

/// The directory `name` in `dir`, made `0700` where missing and opened `O_NOFOLLOW`; a link or a
/// file there is refused, naming `shown`.
fn subdir(dir: &OwnedFd, name: &OsStr, shown: &Path) -> io::Result<OwnedFd> {
    match rustix::fs::mkdirat(dir, name, Mode::from_raw_mode(0o700)) {
        Err(e) if e != Errno::EXIST => return Err(e.into()),
        _ => {}
    }
    rustix::fs::openat(dir, name, DIR, Mode::empty()).map_err(|e| {
        if e == Errno::LOOP || e == Errno::NOTDIR {
            io::Error::other(format!(
                "{} is a link or a file where marion keeps a directory; refusing to write below it",
                shown.display()
            ))
        } else {
            e.into()
        }
    })
}

fn regular(file: File, shown: &Path) -> io::Result<File> {
    if file.metadata()?.is_file() {
        Ok(file)
    } else {
        Err(io::Error::other(format!(
            "{} is not a regular file",
            shown.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    /// An operator's directory holding one file, and a check that it is exactly as it was.
    struct Outside {
        dir: PathBuf,
        dir_mode: u32,
        file: PathBuf,
    }

    impl Outside {
        fn new(root: &Path) -> Self {
            let dir = root.join("operator");
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join(".zshrc");
            std::fs::write(&file, b"operator's\n").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
            Self {
                dir_mode: mode(&dir),
                dir,
                file,
            }
        }

        fn untouched(&self) {
            assert_eq!(std::fs::read(&self.file).unwrap(), b"operator's\n");
            assert_eq!(mode(&self.file), 0o644, "never chmodded");
            assert_eq!(mode(&self.dir), self.dir_mode);
            assert_eq!(
                std::fs::read_dir(&self.dir).unwrap().count(),
                1,
                "nothing made beside it"
            );
        }
    }

    /// **Documents, appends and truncations land in the agent dir, owner-only**, with missing
    /// levels made `0700`, an older wider file narrowed, and nothing that leaves the agent dir.
    #[test]
    fn writes_land_below_the_agent_dir_owner_only() {
        let root = marion_testsupport::scratch("agent-fs-writes");
        let agent = root.join("agents/n");
        let fs = AgentFs::open(&agent).unwrap();
        assert_eq!(mode(&agent), 0o700);
        let doc = agent.join("config/a/doc.json");
        fs.replace(&doc, b"{}").unwrap();
        assert_eq!(std::fs::read(&doc).unwrap(), b"{}");
        assert_eq!(mode(&doc), 0o600);
        assert_eq!(mode(&agent.join("config/a")), 0o700);
        assert_eq!(
            std::fs::read_dir(agent.join("config/a")).unwrap().count(),
            1,
            "no staging file survives"
        );

        let log = agent.join("events.jsonl");
        std::fs::write(&log, b"old\n").unwrap();
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
        io::Write::write_all(&mut fs.append(&log).unwrap(), b"new\n").unwrap();
        assert_eq!(std::fs::read(&log).unwrap(), b"old\nnew\n");
        assert_eq!(mode(&log), 0o600, "narrowed through the handle");
        io::Write::write_all(&mut fs.create(&log).unwrap(), b"x").unwrap();
        assert_eq!(std::fs::read(&log).unwrap(), b"x");
        fs.rename(&log, &agent.join("events.jsonl.1")).unwrap();
        assert!(!log.exists() && agent.join("events.jsonl.1").is_file());

        for outside in [
            agent.join("config/../../escaped"),
            root.join("beside"),
            agent.clone(),
        ] {
            assert!(fs.replace(&outside, b"x").is_err(), "{}", outside.display());
            assert!(fs.append(&outside).is_err(), "{}", outside.display());
        }
        assert!(!root.join("escaped").exists() && !root.join("beside").exists());
    }

    /// **A link a node left in its agent dir is never written through, chmodded or made below**:
    /// at a document it is replaced, at a file opened for appending or truncating it is refused,
    /// and in place of a directory (a level under the agent dir, or the agent dir itself) it
    /// refuses the write by name. The operator's file and directory stay byte- and
    /// mode-identical. A FIFO is refused promptly rather than waited on.
    #[test]
    fn a_planted_link_is_replaced_or_refused_and_never_followed() {
        let root = marion_testsupport::scratch("agent-fs-links");
        let outside = Outside::new(&root);
        let agent = root.join("agents/n");
        let fs = AgentFs::open(&agent).unwrap();

        let doc = agent.join("doc.json");
        symlink(&outside.file, &doc).unwrap();
        fs.replace(&doc, b"marion's").unwrap();
        outside.untouched();
        assert!(std::fs::symlink_metadata(&doc).unwrap().is_file());

        let log = agent.join("events.jsonl");
        symlink(&outside.file, &log).unwrap();
        assert!(fs.append(&log).is_err());
        assert!(fs.create(&log).is_err());
        assert!(fs.read(&log).is_err());
        outside.untouched();
        std::fs::remove_file(&log).unwrap();
        std::fs::hard_link(&outside.file, &log).unwrap();
        assert!(fs.append(&log).is_err(), "a hard link is not appended to");
        assert!(fs.create(&log).is_err(), "nor truncated");
        outside.untouched();
        std::fs::remove_file(&log).unwrap();

        let fifo = agent.join("pty.cast");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let started = std::time::Instant::now();
        assert!(fs.create(&fifo).is_err() && fs.read(&fifo).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));

        let config = agent.join("config");
        symlink(&outside.dir, &config).unwrap();
        let err = fs.replace(&config.join("doc.json"), b"x").unwrap_err();
        assert!(
            err.to_string().contains(&config.display().to_string()),
            "{err}"
        );
        assert!(fs.dir(&config.join("deeper")).is_err());
        outside.untouched();

        drop(fs);
        std::fs::rename(&agent, root.join("moved")).unwrap();
        symlink(&outside.dir, &agent).unwrap();
        let err = AgentFs::open(&agent).unwrap_err();
        assert!(
            err.to_string().contains(&agent.display().to_string()),
            "{err}"
        );
        outside.untouched();
    }

    /// **Once open, the agent dir is the one marion opened**: a link swapped in for a level
    /// after the walk began leads nowhere, because every later open is relative to the fd.
    #[test]
    fn a_link_swapped_in_after_open_is_not_followed() {
        let root = marion_testsupport::scratch("agent-fs-swap");
        let outside = Outside::new(&root);
        let agent = root.join("agents/n");
        let fs = AgentFs::open(&agent).unwrap();
        std::fs::rename(&agent, root.join("moved")).unwrap();
        symlink(&outside.dir, &agent).unwrap();
        fs.replace(&agent.join("doc.json"), b"x").unwrap();
        outside.untouched();
        assert_eq!(std::fs::read(root.join("moved/doc.json")).unwrap(), b"x");
    }
}
