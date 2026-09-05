//! The two inputs every fresh id needs: wall-clock milliseconds and a handful of entropy.
//!
//! A leaf on purpose. `run`, `root`, `handler` and `journal` all mint ids, and `journal` sits
//! beneath `run`, so the minting inputs live below both rather than in whichever module happened
//! to need them first.

use std::io::Read;

use marion_core::ids::RAND_BYTES;

/// Fresh entropy for an id, sized for uniqueness inside a UUIDv7.
pub(crate) fn entropy() -> std::io::Result<[u8; RAND_BYTES]> {
    let mut bytes = [0; RAND_BYTES];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Wall-clock milliseconds, the timestamp half of a UUIDv7.
pub(crate) fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
