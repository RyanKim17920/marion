#!/usr/bin/env python3
"""S15: the second question item 25 says the platform-launcher option carries.

§5.7 lets the supervisor exit after an idle grace period with zero clients and
zero non-terminal nodes, and requires that exit to be journaled as a decision.
Item 25 says an external launcher that restarts the supervisor after that exit
"would fight §5.7's lifetime rules, and that conflict is a reason to reject the
option, not a detail to settle later".

That is a testable claim about `launchd`, not an opinion, so this measures it:
a user LaunchAgent whose payload records its own pid/ppid/sid/pgid and then
**exits cleanly with status 0**, exactly as §5.7's idle exit would. If launchd
starts it again, the conflict is real and recorded rather than argued.

Two configurations are run: `KeepAlive` true, and the `KeepAlive` dictionary
form `SuccessfulExit: false`, which is the documented way to ask launchd not to
restart a job that exited cleanly -- the obvious escape hatch, measured rather
than assumed to work.

The agent is bootstrapped into this user's gui domain and **booted out in a
finally**; the label is `com.marion.s15.probe` and the plist lives under
a temp dir (macOS TCC refuses a launchd job under ~/Desktop),
not in ~/Library/LaunchAgents.
"""
import json
import os
import plistlib
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import procid  # noqa: E402

# NOT under the repo: a launchd-spawned process reading or writing under
# ~/Desktop is refused by macOS TCC and the job silently produces nothing. The
# first run of this probe measured exactly that and nothing else.
RUN = os.path.join(tempfile.gettempdir(), "marion-s15-launchd")
LABEL = "com.marion.s15.probe"
REPORT = os.getenv("S15_LAUNCHD_REPORT", os.path.join(HERE, "s15-launchd.json"))

PAYLOAD = '''\
import json, os, sys, time
rec = {"pid": os.getpid(), "ppid": os.getppid(), "pgid": os.getpgid(0),
       "sid": os.getsid(0), "t": time.time()}
with open(sys.argv[1], "a") as fh:
    fh.write(json.dumps(rec) + "\\n")
sys.exit(0)   # a clean exit, exactly as §5.7's idle exit would be
'''


def launchctl(*args):
    p = subprocess.run(["launchctl"] + list(args), capture_output=True, text=True)
    return {"argv": ["launchctl"] + list(args), "rc": p.returncode,
            "stdout": p.stdout.strip()[:400], "stderr": p.stderr.strip()[:400]}


def one(name, keepalive):
    d = os.path.join(RUN, name)
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d, exist_ok=True)
    payload = os.path.join(d, "payload.py")
    log = os.path.join(d, "starts.jsonl")
    with open(payload, "w") as fh:
        fh.write(PAYLOAD)
    plist = os.path.join(d, LABEL + ".plist")
    with open(plist, "wb") as fh:
        plistlib.dump({"Label": LABEL,
                       "ProgramArguments": [sys.executable, payload, log],
                       "RunAtLoad": True,
                       "StandardErrorPath": os.path.join(d, "stderr.txt"),
                       "KeepAlive": keepalive}, fh)

    out = {"config": name, "keepalive": keepalive, "steps": []}
    domain = "gui/%d" % os.getuid()
    try:
        out["steps"].append(launchctl("bootout", domain + "/" + LABEL))  # any stale one
        out["steps"].append(launchctl("bootstrap", domain, plist))
        if out["steps"][-1]["rc"] != 0:
            out["status"] = "UNMEASURED -- bootstrap failed"
            return out
        time.sleep(12)          # long enough for several respawns
        out["steps"].append(launchctl("print", domain + "/" + LABEL))
        starts = [json.loads(x) for x in open(log)] if os.path.exists(log) else []
        out["starts"] = starts
        out["start_count_in_12s"] = len(starts)
        out["restarted_after_clean_exit"] = len(starts) > 1
        if starts:
            out["first_start_identity"] = {
                k: starts[0][k] for k in ("pid", "ppid", "pgid", "sid")}
            out["distinct_pids"] = sorted({s["pid"] for s in starts})
        out["status"] = "ok" if starts else "UNMEASURED -- payload never ran"
        return out
    finally:
        out["steps"].append(launchctl("bootout", domain + "/" + LABEL))
        out["still_loaded_after_bootout"] = (
            launchctl("print", domain + "/" + LABEL)["rc"] == 0)


def identity_run():
    """A long-lived launchd job, so its session and group can be recorded the
    same way every other mechanism's were."""
    d = os.path.join(RUN, "identity")
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d, exist_ok=True)
    state = os.path.join(d, "state.json")
    payload = os.path.join(d, "sleeper.py")
    with open(payload, "w") as fh:
        fh.write('import json, os, sys, time\n'
                 'json.dump({"pid": os.getpid(), "ppid": os.getppid(),\n'
                 '           "pgid": os.getpgid(0), "sid": os.getsid(0)},\n'
                 '          open(sys.argv[1], "w"))\n'
                 'time.sleep(600)\n')
    plist = os.path.join(d, LABEL + ".plist")
    with open(plist, "wb") as fh:
        plistlib.dump({"Label": LABEL,
                       "ProgramArguments": [sys.executable, payload, state],
                       "StandardErrorPath": os.path.join(d, "stderr.txt"),
                       "RunAtLoad": True}, fh)
    domain = "gui/%d" % os.getuid()
    out = {"config": "identity", "steps": []}
    try:
        out["steps"].append(launchctl("bootout", domain + "/" + LABEL))
        out["steps"].append(launchctl("bootstrap", domain, plist))
        deadline = time.time() + 15
        while time.time() < deadline and not os.path.exists(state):
            time.sleep(0.3)
        if not os.path.exists(state):
            out["status"] = "UNMEASURED -- job never reported"
            return out
        out["supervisor"] = json.load(open(state))
        pid = out["supervisor"]["pid"]
        out["ps"] = procid.ps_rows([pid])
        out["is_session_leader"] = out["supervisor"]["sid"] == pid
        out["is_group_leader"] = out["supervisor"]["pgid"] == pid
        out["group_members"] = [r["pid"] for r in procid.ps_rows()
                                if r["pgid"] == out["supervisor"]["pgid"]]
        out["shares_session_with_observer"] = (
            out["supervisor"]["sid"] == os.getsid(0))
        out["status"] = "ok"
        return out
    finally:
        out["steps"].append(launchctl("bootout", domain + "/" + LABEL))


if __name__ == "__main__":
    r = {"host": os.uname().sysname + " " + os.uname().release,
         "label": LABEL,
         "identity": identity_run(),
         "lifetime": [one("keepalive_true", True),
                      one("keepalive_successfulexit_false",
                          {"SuccessfulExit": False})]}
    with open(REPORT, "w") as fh:
        json.dump(r, fh, indent=2)
    print(json.dumps(r, indent=2))
