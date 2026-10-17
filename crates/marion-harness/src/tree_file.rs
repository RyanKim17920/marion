//! **The one way marion reads a file out of a tree a node or a repository controls**: a
//! repository's `.mcp.json`, its claude settings, its agent-types table, a worktree's `.git`.
//!
//! Whatever is at such a path is the tree's choice, and marion's process reads it: a FIFO there
//! would block a plain `open` for ever, a link to `/dev/zero` or a sparse 64 GiB file would hold
//! a plain read until memory ran out. [`read`] opens with `O_NOFOLLOW | O_NONBLOCK`, takes only a
//! regular file and stops past [`CAP`] bytes; anything else is an error, which every caller turns
//! into a refusal or "not trusted", never a guess. A missing file stays
//! [`std::io::ErrorKind::NotFound`], so a caller can still tell "absent" from "refused".
//!
//! A caller that accepts an in-tree link (the agent-types table) checks where it points and then
//! reads the resolved path, so the final open still follows nothing.

use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// The most marion reads of one tree file: every document read this way is a small config.
pub const CAP: u64 = 1 << 20;

/// The bytes of the regular file at `path`, at most [`CAP`] of them, as the module doc says.
pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    read_capped(path, CAP)
}

/// [`read`] with its own `cap`, for a caller that knows how long the file must be (a committed
/// blob's size) and so refuses one byte more.
pub fn read_capped(path: &Path, cap: u64) -> io::Result<Vec<u8>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if meta.len() > cap {
        return Err(too_long(path, cap));
    }
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes)?;
    // It may have grown since `metadata`.
    if bytes.len() as u64 > cap {
        return Err(too_long(path, cap));
    }
    Ok(bytes)
}

fn too_long(path: &Path, cap: u64) -> io::Error {
    io::Error::other(format!("{} is longer than {cap} bytes", path.display()))
}

/// [`read`], as UTF-8 text.
pub fn read_to_string(path: &Path) -> io::Result<String> {
    String::from_utf8(read(path)?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A small regular file is read; a FIFO, a link and an oversize file are refused, and
    /// promptly; a missing file is still `NotFound`.** The FIFO has no writer, so a blocking open
    /// would hang this test rather than fail it.
    #[test]
    fn a_tree_read_takes_a_small_regular_file_and_refuses_anything_planted() {
        let dir = marion_testsupport::scratch("tree-file");
        let good = dir.join("good.json");
        std::fs::write(&good, "{}").unwrap();
        assert_eq!(read_to_string(&good).unwrap(), "{}");

        let fifo = dir.join("fifo.json");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let started = std::time::Instant::now();
        assert!(read(&fifo).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));

        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&good, &link).unwrap();
        assert!(read(&link).is_err(), "a link is not followed");
        std::os::unix::fs::symlink("/dev/zero", dir.join("zero.json")).unwrap();
        assert!(read(&dir.join("zero.json")).is_err());

        let big = dir.join("big.json");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(64 << 30).unwrap();
        assert!(read(&big).is_err(), "a sparse 64 GiB file is refused");
        std::fs::write(&big, vec![b' '; CAP as usize]).unwrap();
        assert_eq!(
            read(&big).unwrap().len() as u64,
            CAP,
            "exactly the cap is read"
        );
        let short = dir.join("short");
        std::fs::write(&short, [b'x'; 64]).unwrap();
        assert_eq!(read_capped(&short, 64).unwrap().len(), 64, "a stated cap");
        std::fs::write(&short, [b'x'; 65]).unwrap();
        assert!(
            read_capped(&short, 64).is_err(),
            "one byte past a stated cap"
        );

        assert_eq!(
            read(&dir.join("absent.json")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(read(&dir).is_err(), "a directory is refused");
    }
}
