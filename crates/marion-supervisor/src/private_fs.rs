//! **marion's state is the operator's alone**: every directory it makes under the state root is
//! `0700` and every document it writes there is `0600`.
//!
//! The state tree names each node, its task, its workspace and its transcript, and a config
//! document can carry a capability token. `create_dir_all` and a plain `OpenOptions` leave both to
//! the umask, which on a stock system grants every local user read access. One place decides the
//! modes so a new writer cannot forget them.
//!
//! It is also the one place a document is *replaced*: [`write_atomic`] is the only way marion
//! rewrites a whole file it keeps (a contract, a config document, an index), so a reader or a crash
//! sees the old document or the new one and never a truncated middle.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// `create_dir_all`, with every directory it creates made `0700`. A directory that already exists
/// keeps its mode: an operator who widened one did so on purpose.
pub(crate) fn create_dir_all(path: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// Open `path` for appending, creating it `0600` and its missing parents `0700`.
///
/// A file an older marion created `0644` is narrowed on open, through the handle, so a journal or
/// event stream that predates this rule stops being readable the next time marion writes to it.
pub(crate) fn open_append(path: &Path) -> io::Result<File> {
    open(path, OpenOptions::new().append(true))
}

/// Create or truncate `path` for writing, `0600`, with its missing parents `0700`, narrowing a file
/// that already existed with a wider mode the same way [`open_append`] does.
pub(crate) fn create(path: &Path) -> io::Result<File> {
    open(path, OpenOptions::new().write(true).truncate(true))
}

fn open(path: &Path, options: &mut OpenOptions) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        create_dir_all(dir)?;
    }
    let file = options.create(true).mode(0o600).open(path)?;
    // `mode` applies only to a file this call creates.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Replace `path` with `bytes`, `0600`, creating missing parents `0700`.
///
/// Through a sibling staging file opened `O_EXCL` under an unguessable name, `fsync`ed, renamed
/// over `path`, and then the directory `fsync`ed so the rename itself survives a crash. A staging
/// file is removed on any failure. Truncating `path` and writing in place is what this replaces:
/// a crash between the two left an empty document, and a concurrent reader saw a prefix.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        create_dir_all(dir)?;
    }
    replace(path, bytes, 0o600)
}

/// [`write_atomic`] for a document that lives in the operator's repository rather than marion's
/// state (`.marion/agents.toml`): the same replacement, with the umask's modes, because the file is
/// the project's and holds no secret.
pub(crate) fn write_atomic_shared(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    replace(path, bytes, 0o666)
}

fn replace(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let staged = dir.join(format!(
        ".{name}.{}.{}",
        std::process::id(),
        nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let written = (|| -> io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&staged)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&staged, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
        return written;
    }
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_testsupport::scratch;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    /// Mutation: drop the `set_permissions` and a pre-existing `0644` file stays `0644`.
    #[test]
    fn an_existing_world_readable_file_is_narrowed_on_open() {
        let dir = scratch("private-fs-narrow");
        let path = dir.join("journal.jsonl");
        std::fs::write(&path, b"old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        open_append(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"old\n",
            "append, never truncate"
        );
    }

    #[test]
    fn a_created_file_is_truncated_and_owner_only() {
        let dir = scratch("private-fs-create");
        let path = dir.join("nested").join("cast");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"a longer old body\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::io::Write::write_all(&mut create(&path).unwrap(), b"new\n").unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
    }

    #[test]
    fn a_replaced_document_is_whole_owner_only_and_leaves_no_staging_file() {
        let dir = scratch("private-fs-atomic");
        let path = dir.join("fresh").join("doc.json");
        write_atomic(&path, b"first").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert_eq!(mode(&path), 0o600, "the replacement carries its own mode");
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    /// Mutation: drop the `remove_file` and the failed rename leaves its staging file behind.
    #[test]
    fn a_failed_replacement_keeps_the_old_target_and_cleans_up() {
        let dir = scratch("private-fs-atomic-fail");
        let target = dir.join("occupied");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("inside"), b"kept").unwrap();
        assert!(
            write_atomic(&target, b"new").is_err(),
            "rename over a non-empty dir"
        );
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["occupied"], "no staging file survives");
        assert_eq!(std::fs::read(target.join("inside")).unwrap(), b"kept");
    }

    /// The repository document is replaced the same way; its mode is the umask's, which a test
    /// cannot read without racing every other test's file creation, so only the replacement is
    /// asserted here.
    #[test]
    fn a_shared_document_is_replaced_whole() {
        let dir = scratch("private-fs-atomic-shared");
        let path = dir.join(".marion").join("agents.toml");
        write_atomic_shared(&path, b"old").unwrap();
        write_atomic_shared(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn every_created_level_is_owner_only() {
        let dir = scratch("private-fs-levels");
        let deep = dir.join("a").join("b").join("c");
        create_dir_all(&deep).unwrap();
        for p in [dir.join("a"), dir.join("a/b"), deep] {
            assert_eq!(mode(&p), 0o700, "{}", p.display());
        }
    }
}
