//! **Small readings the native-facade suites share**: a launch value as the wire carries it, and
//! an operator terminal's line discipline.

use std::ffi::OsStr;
use std::os::fd::{AsFd, OwnedFd};

use marion_core::proto::OpaqueOsValueV1;
use marion_supervisor::pty::PtyMaster;

/// `value` as a native launch context carries it. Infallible on Unix, which preserves the bytes.
pub fn opaque(value: &OsStr) -> OpaqueOsValueV1 {
    OpaqueOsValueV1::from_os_str(value).expect("Unix preserves native launch bytes")
}

/// The terminal's line discipline, read through a slave opened for the read and closed again, so
/// no extra slave outlives the client (a lingering one would keep the master from seeing EOF).
pub fn termios_of(master: &PtyMaster) -> String {
    let probe: OwnedFd = master.open_slave().expect("termios probe slave");
    format!("{:?}", rustix::termios::tcgetattr(probe.as_fd()).unwrap())
}
