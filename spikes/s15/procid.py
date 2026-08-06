#!/usr/bin/env python3
"""Shared measurement primitives for spike S15.

Everything in here exists to make a *false pass* impossible, so each function
states the failure it is guarding against.

Three-valued liveness. `kill(pid, 0)` returning non-zero is NOT a death:
`EPERM` means the process exists and is not ours. This crate's own
`marion-testsupport::alive` documents that trap and this spike must not
re-open it, so liveness here is ALIVE / DEAD / UNKNOWN and only `ESRCH` maps
to DEAD.

Pid-reuse guard. A pid that reads ALIVE after a kill is only evidence of a
survivor if it is the *same* process. `ps -o lstart=` gives a start timestamp;
comparing it against the one recorded before the kill turns a recycled pid
into UNKNOWN instead of a phantom survivor.
"""
import errno
import os
import subprocess

ALIVE = "ALIVE"
DEAD = "DEAD"
UNKNOWN = "UNKNOWN"
#: Signalled and gone, but not yet reaped by its parent. `kill(pid, 0)` says
#: ALIVE for a zombie, so a run that did not look at `ps`'s state column would
#: report the supervisor's own killed child as a *survivor* -- a false failure
#: in one direction and, for anything the observer does not own, a false pass in
#: the other. This is the reading that separates them.
ZOMBIE = "ZOMBIE"

PS_FMT = "pid=,ppid=,pgid=,sess=,stat=,tty=,lstart=,command="


def liveness(pid):
    """ALIVE / DEAD / UNKNOWN for one pid. Only ESRCH is a death."""
    try:
        os.kill(pid, 0)
        return ALIVE
    except OSError as e:
        if e.errno == errno.ESRCH:
            return DEAD
        return UNKNOWN


def ps_rows(pids=None):
    """`ps` rows for `pids` (or every process when None), as dicts.

    A `ps` that fails to answer is a failure of the measurement, not an empty
    world: callers must treat [] as unmeasured when they asked about pids they
    know exist.
    """
    argv = ["ps", "-o", PS_FMT]
    if pids is None:
        argv.append("-ax")
    else:
        pids = sorted({int(p) for p in pids})
        if not pids:
            return []
        argv += ["-p", ",".join(str(p) for p in pids)]
    out = subprocess.run(argv, capture_output=True, text=True)
    rows = []
    for line in out.stdout.splitlines():
        f = line.split(None, 6)
        if len(f) < 7:
            continue
        try:
            pid, ppid, pgid, sess = int(f[0]), int(f[1]), int(f[2]), int(f[3])
        except ValueError:
            continue
        # lstart is a fixed-width 24-char ctime string; the command follows it.
        rest = f[6]
        lstart, command = rest[:24].strip(), rest[24:].strip()
        try:
            sid = os.getsid(pid)
        except OSError:
            sid = None
        rows.append({"pid": pid, "ppid": ppid, "pgid": pgid, "sess_ps": sess,
                     "sid": sid, "stat": f[4], "tty": f[5], "lstart": lstart,
                     "command": command[:160]})
    return rows


def identify(pids):
    """{pid: lstart} for the pid-reuse guard, taken while everything is alive."""
    return {r["pid"]: r["lstart"] for r in ps_rows(pids)}


def verify(pids, before_lstart):
    """Post-signal state per pid, with the pid-reuse guard applied.

    Returns {pid: state}. A pid whose `kill(pid,0)` says ALIVE but whose
    `lstart` no longer matches the one recorded before the signal is reported
    UNKNOWN: the pid was recycled and this process is not the one we watched.
    """
    now = {r["pid"]: r for r in ps_rows(pids)}
    out = {}
    for pid in pids:
        state = liveness(pid)
        if state == ALIVE:
            was, row = before_lstart.get(pid), now.get(pid)
            if row is None:
                # kill() says reachable but ps cannot see it -- do not guess.
                state = UNKNOWN
            elif was is not None and was != row["lstart"]:
                state = UNKNOWN  # pid reuse: not the process we enumerated
            elif row["stat"].startswith("Z"):
                state = ZOMBIE
        out[str(pid)] = state
    return out


def descendant_pids(rows, root):
    """run.rs `descendant_pids`, reimplemented so the spike measures marion's
    actual algorithm and not a lookalike: root, the rest of root's group, and
    every descendant of those by ppid."""
    by_pid = {r["pid"]: r for r in rows}
    seen = [root]
    root_pgid = by_pid.get(root, {}).get("pgid")
    if root_pgid is not None:
        for r in rows:
            if r["pgid"] == root_pgid and r["pid"] not in seen:
                seen.append(r["pid"])
    i = 0
    while i < len(seen):
        cur = seen[i]
        for r in rows:
            if r["ppid"] == cur and r["pid"] not in seen:
                seen.append(r["pid"])
        i += 1
    return seen


def pgids_of(rows, pids):
    """run.rs `pgids_of`."""
    by_pid = {r["pid"]: r for r in rows}
    out = []
    for pid in pids:
        g = by_pid.get(pid, {}).get("pgid")
        if g is not None and g not in out:
            out.append(g)
    return out


def signal_targets(pgids, own_pgid):
    """run.rs `signal_targets`: never group 0, never group 1, never our own.

    The third exclusion is the clause §11 item 25 calls the hinge, so this
    spike reproduces it exactly rather than approximating it.
    """
    return [g for i, g in enumerate(pgids)
            if g > 1 and g != own_pgid and g not in pgids[:i]]


def selfid(label):
    return {"label": label, "pid": os.getpid(), "ppid": os.getppid(),
            "pgid": os.getpgid(0), "sid": os.getsid(0)}
