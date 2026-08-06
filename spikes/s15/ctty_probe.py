#!/usr/bin/env python3
"""S15 tiebreak: does the session-leader variant acquire a controlling terminal
by accident?

`setsid_leader` and `setsid_double` measure identically on everything item 25
asks about -- singleton process group, reparented to pid 1, no session or group
shared with the launcher. The only difference is whether the supervisor *leads*
its session, and the consequence of leading is not visible in `ps` until
something opens a tty. So this opens one.

Each mechanism launches a lite supervisor that creates a pty and opens the
slave **without** `O_NOCTTY`. The measurement is the supervisor's `ps` tty
column afterwards: `??` means it acquired nothing, a device name means it now
has a controlling terminal it never asked for -- and a controlling terminal is
a channel through which a hangup can reach a supervisor that is supposed to be
detached.
"""
import json
import os
import shutil
import signal
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import procid  # noqa: E402

RUN = os.path.join(HERE, "run")
REPORT = os.getenv("S15_CTTY_REPORT", os.path.join(HERE, "s15-ctty.json"))


def one(mech):
    d = os.path.join(RUN, "ctty-" + mech)
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(os.path.join(d, "wt"), exist_ok=True)
    state = os.path.join(d, "state.json")
    env = dict(os.environ)
    env.update({"S15_STATE": state, "S15_LAUNCHER": os.path.join(d, "launcher.json"),
                "S15_CMD": os.path.join(d, "cmd"),
                "S15_ACTED": os.path.join(d, "acted.json"),
                "S15_WT": os.path.join(d, "wt"),
                "S15_CODEX_HOME": os.path.join(d, "codex-home"),
                "S15_PIDFILE": os.path.join(d, "a"),
                "S15_RUNAWAY_PIDFILE": os.path.join(d, "b"),
                "S15_LITE": "1", "S15_OPEN_TTY": "1"})
    out = {"mechanism": mech}
    subprocess.run([sys.executable, os.path.join(HERE, "launch.py"), mech],
                   env=env, check=True)
    deadline, sup = time.time() + 20, None
    while time.time() < deadline:
        if os.path.exists(state):
            try:
                sup = json.load(open(state))
                break
            except (ValueError, OSError):
                pass
        time.sleep(0.2)
    if not sup:
        out["status"] = "UNMEASURED -- supervisor never reported"
        return out
    pid = sup["supervisor"]["pid"]
    rows = procid.ps_rows([pid])
    out["supervisor"] = sup["supervisor"]
    out["opened"] = sup.get("opened_tty", {}).get("name")
    out["is_session_leader"] = sup["supervisor"]["sid"] == pid
    out["tty_after_open"] = rows[0]["tty"] if rows else None
    out["acquired_controlling_terminal"] = bool(
        rows and rows[0]["tty"] not in ("??", "-", ""))
    out["status"] = "ok"
    try:
        os.kill(pid, signal.SIGKILL)
    except OSError:
        pass
    return out


if __name__ == "__main__":
    r = {"host": os.uname().sysname + " " + os.uname().release,
         "runs": [one(m) for m in ("setsid_leader", "setsid_double", "inherit")]}
    with open(REPORT, "w") as fh:
        json.dump(r, fh, indent=2)
    print(json.dumps(r, indent=2))
