//! **Which node's process tree a connection comes from**, so §5.4's token is good only from inside
//! the tree marion launched for its node.
//!
//! A node token is a file any same-uid process can read on Linux, where marion's Landlock layer
//! limits writes alone. A sibling that reads one is, to the token check alone, the node it stole
//! from. So a node's `session/hello` must also come from a process that **descends from that
//! node's root** — the process marion spawned and still holds unreaped, so its pid cannot be
//! recycled while the node is live. A root is matched on its start as well as its pid ([`Root`]),
//! so the moment between its reap and its node being marked finished grants nothing. A sibling cannot make its own process a descendant of
//! another tree: Linux and macOS reparent an orphan to init or a subreaper, never into a tree of
//! the orphan's choosing.
//!
//! The walk is over `ppid` alone, never a process group or session: structured nodes share the
//! detached supervisor's session, so a sibling may `setpgid` into another node's group, and codex
//! `setsid`s each tool-call child (S7), so a session check would refuse a node's own descendants.
//!
//! **Each hop is a sandwich** ([`first_root`]): read the child's parent and start, read the parent,
//! then read the child again and require the same parent and the same start, and the parent no
//! younger than the child. A dying parent reparents its children at its exit, before its pid can
//! be reaped and reused, so a child still naming it after it was read proves the parent read was
//! the real one.
//!
//! **The first node root the walk meets decides**, and the supervisor's own pid refuses: a
//! supervisor started from inside a node, or a node whose harness spawned another node's root,
//! would otherwise let every node below "descend" from the one above it.
//!
//! **The peer itself** is pinned at accept ([`AcceptedPeer`]): `SO_PEERPIDFD` where the kernel has
//! it (Linux 6.5), `pidfd_open` at accept otherwise, and on every platform a peer that started
//! after the connection was accepted is refused — a pid recycled after its connector left is
//! younger than the accept. What remains is a recycle inside the connect-to-accept backlog, where
//! the attacker cannot pick who receives the pid.
//!
//! [`first_root`]: crate::peer_tree::first_root
//! [`AcceptedPeer`]: crate::peer_tree::AcceptedPeer
//! [`Root`]: crate::peer_tree::Root

use crate::procid::Hop;

/// **A node's root as recorded when its process was spawned**: the pid, and the start it had then.
/// A walk takes a pid for that root only while it still wears that start, so a pid the kernel gave
/// to another process after the root was reaped — while its node is still listed as running — is
/// not the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Root {
    pub(crate) pid: i32,
    /// [`Hop::start`] at recording.
    pub(crate) start: u64,
}

impl Root {
    /// `pid` as a root, read now; `None` where its start cannot be read, which lists no root and
    /// so lets nothing in through it.
    pub(crate) fn of(pid: i32) -> Option<Root> {
        crate::procid::hop(pid).map(|h| Root {
            pid,
            start: h.start,
        })
    }
}

/// How many parents a walk reads before it gives up. A node's bridge sits two or three below its
/// root; a deeper chain is not a bridge.
const MAX_HOPS: usize = 64;

/// The connecting process, as the kernel named it when the connection was accepted.
#[derive(Debug)]
pub(crate) struct AcceptedPeer {
    pid: i32,
    /// [`crate::procid::start_clock_now`] at accept; `None` where it could not be read, which
    /// refuses.
    accepted: Option<u64>,
    /// The peer pinned as a process, where the kernel can (Linux).
    #[cfg(target_os = "linux")]
    pidfd: Option<std::os::fd::OwnedFd>,
}

impl AcceptedPeer {
    /// Pin the peer of `stream`, whose kernel-reported pid is `pid`, now.
    pub(crate) fn at_accept(
        stream: &std::os::unix::net::UnixStream,
        pid: Option<u32>,
    ) -> Option<Self> {
        let pid = i32::try_from(pid?).ok().filter(|p| *p > 1)?;
        #[cfg(not(target_os = "linux"))]
        let _ = stream;
        Some(Self {
            pid,
            accepted: crate::procid::start_clock_now(),
            #[cfg(target_os = "linux")]
            pidfd: linux::peer_pidfd(stream, pid),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(pid: i32, accepted: Option<u64>) -> Self {
        Self {
            pid,
            accepted,
            #[cfg(target_os = "linux")]
            pidfd: None,
        }
    }

    pub(crate) fn pid(&self) -> i32 {
        self.pid
    }

    /// The first of `roots` the peer descends from, read from the live process table, or why
    /// there is none. `supervisor` is this process's pid.
    pub(crate) fn root_of<K: Clone>(
        &self,
        roots: &[(Root, K)],
        supervisor: i32,
    ) -> Result<K, String> {
        let found = first_root(
            self.pid,
            self.accepted,
            roots,
            supervisor,
            crate::procid::hop,
        )?;
        if self.still_the_peer() {
            Ok(found)
        } else {
            Err(format!(
                "process {} is no longer the process that connected, so its ancestry says nothing \
                 about the connection",
                self.pid
            ))
        }
    }

    /// Whether the pid walked is still the process that connected. Linux asks the pidfd; where
    /// there is none, the start-before-accept bound [`first_root`] applies is the whole check.
    fn still_the_peer(&self) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(fd) = &self.pidfd {
            return linux::pidfd_names(fd, self.pid);
        }
        true
    }
}

/// **The first of `roots` that `peer` is or descends from**, walking `ppid` with the sandwich the
/// module doc describes and `read` for each step. `accepted` is when the connection was accepted,
/// in [`Hop::start`]'s units: a peer that started later is not the one that connected.
pub(crate) fn first_root<K: Clone>(
    peer: i32,
    accepted: Option<u64>,
    roots: &[(Root, K)],
    supervisor: i32,
    read: impl Fn(i32) -> Option<Hop>,
) -> Result<K, String> {
    let live = |pid: i32| match read(pid) {
        Some(hop) if !hop.zombie => Ok(hop),
        Some(_) => Err(format!("process {pid} is a zombie")),
        None => Err(format!("process {pid} could not be read")),
    };
    let mut child = peer;
    let mut seen = live(child)?;
    let Some(accepted) = accepted else {
        return Err("the connection's accept time could not be read".into());
    };
    if seen.start > accepted {
        return Err(format!(
            "process {peer} started after the connection was accepted, so it is not the process \
             that connected"
        ));
    }
    for _ in 0..MAX_HOPS {
        if let Some((_, key)) = roots
            .iter()
            .find(|(root, _)| root.pid == child && root.start == seen.start)
        {
            return Ok(key.clone());
        }
        if child == supervisor {
            return Err(format!(
                "process {peer} descends from this supervisor without passing through a node's root"
            ));
        }
        let parent = seen.ppid;
        if parent <= 1 {
            return Err(format!(
                "process {peer} is not below any live node's root (its ancestry ends at pid {parent})"
            ));
        }
        let above = live(parent)?;
        let again = live(child)?;
        if again.ppid != parent || again.start != seen.start {
            return Err(format!(
                "process {child}'s parent changed while its ancestry was read"
            ));
        }
        if above.start > seen.start {
            return Err(format!(
                "process {parent} started after its child {child}, so it is a recycled pid and not \
                 the parent"
            ));
        }
        child = parent;
        seen = above;
    }
    Err(format!(
        "process {peer}'s ancestry is deeper than {MAX_HOPS} processes"
    ))
}

#[cfg(target_os = "linux")]
mod linux {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    /// `SO_PEERPIDFD` (Linux 6.5, `asm-generic/socket.h`, the same on every architecture marion
    /// builds for); `pidfd_open(2)` of the pid at accept where the kernel has no such option.
    pub(super) fn peer_pidfd(stream: &std::os::unix::net::UnixStream, pid: i32) -> Option<OwnedFd> {
        unsafe extern "C" {
            fn getsockopt(fd: i32, level: i32, name: i32, value: *mut i32, len: *mut u32) -> i32;
        }
        const SOL_SOCKET: i32 = 1;
        const SO_PEERPIDFD: i32 = 77;
        let mut fd: i32 = -1;
        let mut len = std::mem::size_of::<i32>() as u32;
        // SAFETY: `fd` and `len` are live locals of the sizes passed, and the stream outlives the
        // call. On success the kernel installed a new descriptor this process now owns.
        let rc = unsafe {
            getsockopt(
                stream.as_raw_fd(),
                SOL_SOCKET,
                SO_PEERPIDFD,
                &mut fd,
                &mut len,
            )
        };
        if rc == 0 && fd >= 0 {
            // SAFETY: a descriptor the kernel just created for this process, owned by nobody else.
            return Some(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let pid = rustix::process::Pid::from_raw(pid)?;
        rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).ok()
    }

    /// Whether pidfd `fd` still names live process `pid`: its `fdinfo` `Pid:` line, which reads
    /// `-1` once the process has exited.
    pub(super) fn pidfd_names(fd: &OwnedFd, pid: i32) -> bool {
        let Ok(info) = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd()))
        else {
            return false;
        };
        info.lines()
            .find_map(|l| l.strip_prefix("Pid:"))
            .and_then(|v| v.trim().parse::<i32>().ok())
            == Some(pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A process table: pid → (ppid, start, zombie).
    fn table(rows: &[(i32, i32, u64)]) -> HashMap<i32, Hop> {
        rows.iter()
            .map(|&(pid, ppid, start)| {
                (
                    pid,
                    Hop {
                        ppid,
                        start,
                        zombie: false,
                    },
                )
            })
            .collect()
    }

    const SUP: i32 = 10;

    /// supervisor 10 → root A 20 → harness bridge 21 → setsid'd tool 22; root B 30 → 31.
    fn fleet() -> HashMap<i32, Hop> {
        table(&[
            (10, 1, 5),
            (20, 10, 10),
            (21, 20, 11),
            (22, 21, 12),
            (30, 10, 20),
            (31, 30, 21),
            (40, 1, 30),
        ])
    }

    fn roots() -> Vec<(Root, &'static str)> {
        vec![
            (Root { pid: 20, start: 10 }, "A"),
            (Root { pid: 30, start: 20 }, "B"),
        ]
    }

    fn walk(t: &HashMap<i32, Hop>, peer: i32) -> Result<&'static str, String> {
        first_root(peer, Some(1_000), &roots(), SUP, |p| t.get(&p).copied())
    }

    /// **A listed root's pid that now wears another start is not that root**: A's root was reaped
    /// and 20 handed to a newer process while A is still listed, so 21 below it is in no tree.
    ///
    /// Mutation: match a root on its pid alone and 21 is taken as A's.
    #[test]
    fn a_listed_roots_pid_wearing_another_start_is_not_that_root() {
        let t = fleet();
        let stale = [(Root { pid: 20, start: 9 }, "A"), roots()[1]];
        let refused = first_root(21, Some(1_000), &stale, SUP, |p| t.get(&p).copied());
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(
            walk(&t, 21),
            Ok("A"),
            "the same pid at its recorded start is A"
        );
    }

    #[test]
    fn a_descendant_finds_its_own_root_and_a_sibling_finds_its_own() {
        let t = fleet();
        assert_eq!(walk(&t, 21), Ok("A"));
        assert_eq!(
            walk(&t, 22),
            Ok("A"),
            "a setsid'd grandchild is still below A"
        );
        assert_eq!(walk(&t, 20), Ok("A"), "the root itself");
        assert_eq!(walk(&t, 31), Ok("B"));
    }

    #[test]
    fn a_process_below_no_root_is_refused() {
        let t = fleet();
        let orphan = walk(&t, 40).unwrap_err();
        assert!(orphan.contains("not below any live node"), "{orphan}");
        let mut t = fleet();
        t.insert(
            50,
            Hop {
                ppid: SUP,
                start: 40,
                zombie: false,
            },
        );
        let helper = walk(&t, 50).unwrap_err();
        assert!(helper.contains("this supervisor"), "{helper}");
    }

    /// The nesting rule: node B's root runs below node A's tree (a harness that started another
    /// node). B's process meets B's root first, so it is B's, never A's.
    #[test]
    fn the_first_root_on_the_walk_decides_so_a_nested_node_is_not_its_hosts() {
        let t = table(&[
            (10, 1, 5),
            (20, 10, 10),
            (21, 20, 11),
            (30, 21, 20),
            (31, 30, 21),
        ]);
        assert_eq!(walk(&t, 31), Ok("B"));
        assert_ne!(walk(&t, 31), Ok("A"));
    }

    #[test]
    fn a_zombie_an_unreadable_hop_or_a_bottomless_chain_refuses() {
        let mut t = fleet();
        t.get_mut(&21).unwrap().zombie = true;
        assert!(walk(&t, 22).unwrap_err().contains("zombie"));
        let mut t = fleet();
        t.remove(&21);
        assert!(walk(&t, 22).unwrap_err().contains("could not be read"));
        let deep: Vec<(i32, i32, u64)> = (100..300).map(|p| (p, p + 1, 1)).collect();
        let t = table(&deep);
        assert!(walk(&t, 100).unwrap_err().contains("deeper"));
    }

    #[test]
    fn a_parent_younger_than_its_child_is_a_recycled_pid() {
        let mut t = fleet();
        t.get_mut(&21).unwrap().start = 50;
        assert!(walk(&t, 22).unwrap_err().contains("recycled"));
    }

    #[test]
    fn a_peer_that_started_after_the_accept_is_not_the_connector() {
        let t = fleet();
        let refused = first_root(21, Some(10), &roots(), SUP, |p| t.get(&p).copied()).unwrap_err();
        assert!(refused.contains("after the connection"), "{refused}");
        let unread = first_root(21, None, &roots(), SUP, |p| t.get(&p).copied()).unwrap_err();
        assert!(unread.contains("accept time"), "{unread}");
    }

    /// The sandwich: the child's parent changes between the two reads of it (its parent died and
    /// it was reparented), so the parent read may be a recycled pid's, and the walk refuses.
    #[test]
    fn a_child_whose_parent_changed_between_reads_refuses() {
        let t = fleet();
        let reads = Mutex::new(HashMap::<i32, u32>::new());
        let racing = |p: i32| {
            let mut n = reads.lock().unwrap();
            let k = n.entry(p).or_default();
            *k += 1;
            let mut hop = t.get(&p).copied()?;
            if p == 22 && *k >= 2 {
                hop.ppid = 1;
            }
            Some(hop)
        };
        let refused = first_root(22, Some(1_000), &roots(), SUP, racing).unwrap_err();
        assert!(refused.contains("changed"), "{refused}");
    }

    /// The live table: this test process descends from neither a fake root nor anything but its
    /// own parent, and finds its parent when its parent is named a root.
    #[test]
    fn the_live_table_reads_this_process_and_its_parent() {
        let me = std::process::id() as i32;
        let parent = crate::procid::hop(me).expect("this process reads").ppid;
        let peer = AcceptedPeer::for_test(me, crate::procid::start_clock_now());
        let root = Root::of(parent).expect("the parent reads");
        assert_eq!(peer.root_of(&[(root, "parent")], -1), Ok("parent"));
        let nobody = Root {
            pid: i32::MAX,
            start: 0,
        };
        assert!(peer.root_of(&[(nobody, "nobody")], -1).is_err());
    }

    /// Linux: a pidfd names its process while it lives, and stops once it has exited.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_pidfd_names_its_process_until_it_exits() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let fd = rustix::process::pidfd_open(
            rustix::process::Pid::from_raw(pid).unwrap(),
            rustix::process::PidfdFlags::empty(),
        )
        .unwrap();
        assert!(linux::pidfd_names(&fd, pid));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!linux::pidfd_names(&fd, pid));
    }
}
