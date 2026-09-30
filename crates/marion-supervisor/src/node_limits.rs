//! **The resource ceilings every node starts under**: open files and processes, the same for every
//! row.
//!
//! A node runs whatever its model decides to run, so a runaway loop — a fork bomb, a descriptor
//! leak — must meet a wall before it meets the operator's machine. Each ceiling lowers a node's
//! inherited limits (soft and hard) to at most the ceiling and never raises them: a hard limit only
//! root can raise stays where it was. The operator raises a ceiling in their own config:
//!
//! ```toml
//! [limits]
//! max_open_files = 16384
//! max_processes = 8192
//! ```
//!
//! `max_processes` is `RLIMIT_NPROC`, which the kernel counts **per user**, not per node: it bounds
//! how many processes the operator's uid may hold once a node forks, so a node's runaway stops at
//! the ceiling instead of at the system's.

use std::process::Command;

use rustix::process::{Resource, Rlimit};

/// The ceiling on a node's open descriptors when the operator states none.
pub const DEFAULT_OPEN_FILES: u64 = 8192;
/// The ceiling on the operator's process count, applied from a node's first fork, when the
/// operator states none.
pub const DEFAULT_PROCESSES: u64 = 4096;

/// The two ceilings, resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeLimits {
    pub open_files: u64,
    pub processes: u64,
}

impl Default for NodeLimits {
    fn default() -> Self {
        NodeLimits {
            open_files: DEFAULT_OPEN_FILES,
            processes: DEFAULT_PROCESSES,
        }
    }
}

/// `limit` lowered to at most `ceiling`, soft and hard; `None` is unlimited.
fn capped(limit: Rlimit, ceiling: u64) -> Rlimit {
    let cap = |v: Option<u64>| Some(v.map_or(ceiling, |v| v.min(ceiling)));
    Rlimit {
        current: cap(limit.current),
        maximum: cap(limit.maximum),
    }
}

/// **Start `command` under `limits`.** The targets are computed here, from this process's own
/// limits (which the child inherits), so the forked child only makes two `setrlimit` calls.
pub(crate) fn apply_to(command: &mut Command, limits: NodeLimits) -> &mut Command {
    use std::os::unix::process::CommandExt;
    let files = capped(
        rustix::process::getrlimit(Resource::Nofile),
        limits.open_files,
    );
    let procs = capped(
        rustix::process::getrlimit(Resource::Nproc),
        limits.processes,
    );
    // SAFETY: `setrlimit` is async-signal-safe and the closure touches only its own copies;
    // its failure is reported as the spawn's.
    unsafe {
        command.pre_exec(move || {
            rustix::process::setrlimit(Resource::Nofile, files)?;
            rustix::process::setrlimit(Resource::Nproc, procs)?;
            Ok(())
        })
    }
}

/// [`apply_to`] with the operator's ceilings ([`crate::user_config::node_limits`]). A config file
/// that cannot be read leaves the defaults, which are the tighter reading, and its error is the
/// one every spawn already reports through `[delegation]`'s read of the same file.
pub(crate) fn apply(command: &mut Command) -> &mut Command {
    let limits = crate::user_config::node_limits().unwrap_or_default();
    apply_to(command, limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ceiling_lowers_a_limit_and_never_raises_one() {
        let limit = |c, m| Rlimit {
            current: c,
            maximum: m,
        };
        assert_eq!(
            capped(limit(Some(256), None), 8192),
            limit(Some(256), Some(8192))
        );
        assert_eq!(
            capped(limit(Some(10_000), Some(20_000)), 8192),
            limit(Some(8192), Some(8192))
        );
        assert_eq!(capped(limit(None, None), 64), limit(Some(64), Some(64)));
    }

    /// **A node starts under the ceilings**, as the node's own shell reads them.
    #[test]
    fn a_node_reads_its_ceilings_as_its_own_hard_limits() {
        let mut probe = Command::new("sh");
        probe.args(["-c", "ulimit -Hn; ulimit -Hu"]);
        let out = apply_to(
            &mut probe,
            NodeLimits {
                open_files: 300,
                processes: 1000,
            },
        )
        .output()
        .expect("run the probe");
        let text = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = text.lines().collect();
        let inherited = rustix::process::getrlimit(Resource::Nproc).maximum;
        let processes = inherited.map_or(1000, |m| m.min(1000)).to_string();
        assert_eq!(lines, vec!["300", processes.as_str()], "{text}");
    }
}
