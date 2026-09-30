//! **The operator's capability**: 32 random bytes, hex, in `<state>/operator.key`, owner-only.
//!
//! A connection that presents it speaks for the operator ([`crate::client_auth`]); one that does
//! not may act only as the node whose token it holds. marion never hands a node this path or its
//! bytes: a node's declaration carries its own token and the socket's path. It is created once, by
//! whichever of the supervisor or a client needs it first (written whole, then linked into place,
//! so two racing creators agree on one key and no reader sees half of one), and never rewritten,
//! so clients started before a supervisor restart keep working.
//!
//! **What it does not stop**: a process of the operator's own uid that reads the file directly. A
//! node's shell is such a process unless an OS sandbox keeps it out of the state root; until one
//! does, the supervisor also refuses the operator's capability from any process descending from a
//! live node.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use marion_core::secret::Secret;

/// The key's file name in the state root.
pub const FILE: &str = "operator.key";

/// Where the key for state root `state` lives.
pub fn path(state: &Path) -> PathBuf {
    state.join(FILE)
}

/// The key under `state`, created (0600, in a 0700 directory) when there is none yet.
pub fn ensure(state: &Path) -> std::io::Result<Secret> {
    if let Some(key) = read(state)? {
        return Ok(key);
    }
    crate::private_fs::create_dir_all(state)?;
    let fresh = crate::handler::mint_token();
    if fresh.expose().is_empty() {
        return Err(std::io::Error::other(
            "no entropy could be read to mint the operator key",
        ));
    }
    // **Written whole under a private name, then linked into place**: `link(2)` refuses an
    // existing name, so of two racing creators exactly one key lands, and no reader ever opens a
    // file that exists but is not yet written.
    // Named by the pid and a per-process count, so two threads of one process that race here stage
    // two files rather than one.
    static STAGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let staged = state.join(format!(
        "{FILE}.{}.{}.tmp",
        std::process::id(),
        STAGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&staged);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staged)?;
    let written = f
        .write_all(fresh.expose().as_bytes())
        .and_then(|()| f.sync_all());
    let linked = written.and_then(|()| std::fs::hard_link(&staged, path(state)));
    let _ = std::fs::remove_file(&staged);
    match linked {
        Ok(()) => Ok(fresh),
        // Another process linked its key between the read and here: theirs is the key.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            read(state)?.ok_or_else(|| std::io::Error::other("the operator key vanished"))
        }
        Err(e) => Err(e),
    }
}

/// The key under `state`, or `None` where there is none yet. A file that is not owner-only, or
/// is not exactly one key, is an error: a key another user could read is no key.
pub fn read(state: &Path) -> std::io::Result<Option<Secret>> {
    use std::os::unix::fs::MetadataExt;
    let p = path(state);
    let mut f = match std::fs::File::open(&p) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = f.metadata()?;
    if meta.mode() & 0o077 != 0 || meta.uid() != crate::socket::own_uid() {
        return Err(std::io::Error::other(format!(
            "{} is readable by someone other than you; remove it and marion will mint a new one",
            p.display()
        )));
    }
    let mut text = String::new();
    f.read_to_string(&mut text)?;
    let key = text.trim();
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(std::io::Error::other(format!(
            "{} does not hold an operator key; remove it and marion will mint a new one",
            p.display()
        )));
    }
    Ok(Some(Secret::new(key.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_minted_once_owner_only_and_read_back() {
        use std::os::unix::fs::PermissionsExt;
        let dir = marion_testsupport::scratch("operator-key");
        let state = dir.join("state");
        assert!(read(&state).unwrap().is_none());
        let first = ensure(&state).unwrap();
        assert_eq!(first.expose().len(), 64);
        assert_eq!(ensure(&state).unwrap(), first, "created once, then read");
        let mode = std::fs::metadata(path(&state))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::set_permissions(path(&state), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read(&state).is_err(), "a key others can read is refused");
    }

    /// **Racing creators agree on one key, and no reader sees half of one** — the supervisor and
    /// its first client both ensure the key at start, often in the same instant.
    #[test]
    fn racing_creators_agree_on_one_key() {
        let dir = marion_testsupport::scratch("operator-key-race");
        let state = dir.join("state");
        let keys: Vec<Secret> = std::thread::scope(|s| {
            let racers: Vec<_> = (0..8)
                .map(|_| s.spawn(|| ensure(&state).unwrap()))
                .collect();
            racers.into_iter().map(|r| r.join().unwrap()).collect()
        });
        assert!(
            keys.iter().all(|k| k == &keys[0]),
            "one key for every racer"
        );
        let left: Vec<_> = std::fs::read_dir(&state)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            left,
            vec![std::ffi::OsString::from(FILE)],
            "no staged file is left"
        );
    }
}
