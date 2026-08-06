#!/usr/bin/env python3
"""The three candidate detach mechanisms of §11 item 25, as runnable code.

Run as `launch.py <mechanism>`; the launcher exits as soon as the supervisor is
started, which is the point -- afterwards nothing in this process tree is the
supervisor's ancestor.

  setsid_leader   one fork, then `setsid` in the child, which then execs the
                  supervisor. The supervisor IS the session leader: sid == pgid
                  == its own pid, no controlling terminal.

  setsid_double   the classic daemon double fork: fork, `setsid`, fork again,
                  the middle process exits. The supervisor is in a new session
                  it does not lead, so it can never acquire a controlling
                  terminal by opening one.

  inherit         an ordinary spawn with the launcher exiting immediately; the
                  supervisor keeps the launcher's session and process group and
                  reparents to pid 1. The launcher first puts itself in a fresh
                  process group and starts a **sibling job** in it. That models
                  the shell job the launching `marion` would be part of, and it
                  is what makes "what else does one tree-wide signal hit"
                  something this spike measures rather than argues.

The launcher's own identity is written to $S15_LAUNCHER so the observer can
state the supervisor's session and group *relative to its launcher*, which is
the relation item 25 asks for.
"""
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import procid  # noqa: E402

SUPERVISOR = os.path.join(HERE, "supervisor.py")


def write(path, obj):
    tmp = path + ".tmp"
    with open(tmp, "w") as fh:
        json.dump(obj, fh, indent=2)
    os.replace(tmp, path)


def exec_supervisor():
    os.execv(sys.executable, [sys.executable, SUPERVISOR])


def main():
    mech = sys.argv[1]
    out = os.environ["S15_LAUNCHER"]

    if mech in ("setsid_leader", "setsid_double"):
        write(out, {"mechanism": mech, "launcher": procid.selfid("launcher")})
        pid = os.fork()
        if pid == 0:
            os.setsid()
            if mech == "setsid_double":
                if os.fork() != 0:
                    os._exit(0)
            exec_supervisor()
        if mech == "setsid_double":
            os.waitpid(pid, 0)   # the middle process exits at once
        return                   # setsid_leader's child execs and runs on

    if mech == "inherit":
        pid = os.fork()
        if pid == 0:
            os.setpgid(0, 0)  # the launching `marion`'s own group
            sibling = subprocess.Popen(["/bin/sleep", "900"],
                                       stdin=subprocess.DEVNULL,
                                       stdout=subprocess.DEVNULL,
                                       stderr=subprocess.DEVNULL)
            sup = subprocess.Popen([sys.executable, SUPERVISOR],
                                   stdin=subprocess.DEVNULL,
                                   stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL)
            write(out, {"mechanism": mech,
                        "launcher": procid.selfid("launcher"),
                        "sibling_job_pid": sibling.pid,
                        "supervisor_pid": sup.pid})
            os._exit(0)      # the launcher leaves; the supervisor reparents.
        os.waitpid(pid, 0)
        return

    sys.exit("unknown mechanism: " + mech)


if __name__ == "__main__":
    main()
