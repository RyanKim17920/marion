#!/usr/bin/env python3
"""S15 side measurement: does the detached supervisor keep the launching
terminal?

Item 25 says the mechanisms "differ in whether the supervisor is a session
leader [and] whether it shares a process group or session with the `marion`
that started it". Sharing a *session* has a consequence no `ps` column states
outright, so this measures it directly instead of asserting it: a real pty is
allocated, a shim makes itself its session leader and claims it as a
controlling terminal, the supervisor is launched inside that session by each
mechanism, and then the master end is **closed** -- a real hangup.

Recorded per mechanism: the supervisor's controlling tty, whether its session
is the terminal's, and who is still alive three seconds after the hangup.
Liveness uses `procid`'s three-valued reading, so an EPERM is never a death.

No codex and no model: the supervisor runs in lite mode.
"""
import fcntl
import json
import os
import pty
import shutil
import signal
import subprocess
import sys
import termios
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import procid  # noqa: E402

RUN = os.path.join(HERE, "run")
REPORT = os.getenv("S15_HANGUP_REPORT", os.path.join(HERE, "s15-hangup.json"))


def one(mech):
    d = os.path.join(RUN, "hangup-" + mech)
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(os.path.join(d, "wt"), exist_ok=True)
    state = os.path.join(d, "state.json")
    launcher = os.path.join(d, "launcher.json")
    env = dict(os.environ)
    env.update({"S15_STATE": state, "S15_LAUNCHER": launcher,
                "S15_CMD": os.path.join(d, "cmd"),
                "S15_ACTED": os.path.join(d, "acted.json"),
                "S15_WT": os.path.join(d, "wt"),
                "S15_CODEX_HOME": os.path.join(d, "codex-home"),
                "S15_PIDFILE": os.path.join(d, "a"),
                "S15_RUNAWAY_PIDFILE": os.path.join(d, "b"),
                "S15_LITE": "1"})

    out = {"mechanism": mech}
    master, slave = pty.openpty()
    shim = os.fork()
    if shim == 0:
        os.setsid()                                   # new session, no ctty yet
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)      # claim the pty as ctty
        os.dup2(slave, 0), os.dup2(slave, 1), os.dup2(slave, 2)
        os.close(master)
        os.close(slave)
        subprocess.run([sys.executable, os.path.join(HERE, "launch.py"), mech],
                       env=env)
        while True:
            time.sleep(3600)
    os.close(slave)

    try:
        deadline = time.time() + 20
        sup = None
        while time.time() < deadline:
            if os.path.exists(state):
                try:
                    sup = json.load(open(state))["supervisor"]
                    break
                except (ValueError, OSError, KeyError):
                    pass
            time.sleep(0.2)
        if sup is None:
            out["status"] = "UNMEASURED -- supervisor never reported"
            return out

        watch = {"session_leader_shim": shim, "supervisor": sup["pid"]}
        rows = {r["pid"]: r for r in procid.ps_rows(watch.values())}
        out["supervisor"] = sup
        out["ps_before_hangup"] = list(rows.values())
        out["shim_sid"] = os.getsid(shim)
        out["supervisor_controlling_tty"] = rows.get(sup["pid"], {}).get("tty")
        out["supervisor_in_terminal_session"] = sup["sid"] == os.getsid(shim)
        lstart = procid.identify(watch.values())
        before = procid.verify(list(watch.values()), lstart)
        out["state_before_hangup"] = {k: before[str(v)] for k, v in watch.items()}
        if any(v != procid.ALIVE for v in out["state_before_hangup"].values()):
            out["status"] = "UNMEASURED -- not alive before the hangup"
            return out

        os.close(master)          # the hangup
        time.sleep(3.0)
        after = procid.verify(list(watch.values()), lstart)
        out["state_after_hangup"] = {k: after[str(v)] for k, v in watch.items()}
        out["status"] = "ok"
        return out
    finally:
        for pid in (sup or {}).get("pid", 0), shim:
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass
        try:
            os.waitpid(shim, 0)
        except OSError:
            pass


if __name__ == "__main__":
    r = {"host": os.uname().sysname + " " + os.uname().release,
         "runs": [one(m) for m in ("setsid_leader", "setsid_double", "inherit")]}
    with open(REPORT, "w") as fh:
        json.dump(r, fh, indent=2)
    print(json.dumps(r, indent=2))
