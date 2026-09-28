//! **marion's state is the operator's alone**: every directory it makes under the state root is
//! `0700` and every document it writes there is `0600`.
//!
//! The state tree names each node, its task, its workspace and its transcript, and a config
//! document can carry a capability token. `create_dir_all` and a plain `OpenOptions` leave both to
//! the umask, which on a stock system grants every local user read access. One place decides the
//! modes so a new writer cannot forget them.

use std::fs::{File, OpenOptions};
use std::io;
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
    fn every_created_level_is_owner_only() {
        let dir = scratch("private-fs-levels");
        let deep = dir.join("a").join("b").join("c");
        create_dir_all(&deep).unwrap();
        for p in [dir.join("a"), dir.join("a/b"), deep] {
            assert_eq!(mode(&p), 0o700, "{}", p.display());
        }
    }
}
