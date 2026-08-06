#!/usr/bin/env python3
"""The stand-in supervisor for spike S15.

This is the process whose session and process group §11 item 25 says are
undefined. It is started by `launch.py` under one of the candidate detach
mechanisms and then plays exactly the role `spikes/s7/run_probe.py` played from
a foreground parent: it spawns `codex exec` in a **fresh process group**
(`setpgid(0,0)`, which is what `marion-supervisor`'s spawn does today) against
the canned provider, and waits until codex is mid-tool-call with a live
tool-call child and grandchild.

It never calls the model. It writes its own identity and codex's to a state
file so an outside observer -- which cannot `waitpid` it, because after
detaching it is nobody's child -- can measure it.

Two duties after that, selected by the observer writing a command file:

  idle      do nothing; the observer will signal the tree from outside.
  two_step  perform §6.7's kill itself, using run.rs's own algorithm including
            the `signal_targets` filter that refuses to signal our own group.
            The point is to find out whether the supervisor survives its own
            tree kill -- the clause item 25 calls the hinge.
"""
import json
import os
import signal
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import procid  # noqa: E402

CONFIG_TOML = '''\
model = "gpt-5.6-sol"
model_provider = "canned"
approval_policy = "never"
sandbox_mode = "workspace-write"

[model_providers.canned]
name = "canned"
base_url = "http://127.0.0.1:%(port)s/v1"
wire_api = "responses"
env_key = "S15_DUMMY_KEY"

[projects."%(wt)s"]
trust_level = "trusted"
'''


def write_atomic(path, obj):
    tmp = path + ".tmp"
    with open(tmp, "w") as fh:
        json.dump(obj, fh, indent=2)
    os.replace(tmp, path)


def read_pidfile(path):
    if not os.path.exists(path):
        return None
    txt = open(path).read()
    if "done" not in txt:
        return None
    d = {}
    for line in txt.splitlines():
        f = line.split()
        if len(f) == 2 and f[1].isdigit():
            d[f[0]] = int(f[1])
    return d


def main():
    state_path = os.environ["S15_STATE"]
    cmd_path = os.environ["S15_CMD"]
    acted_path = os.environ["S15_ACTED"]
    lite = os.environ.get("S15_LITE") == "1"
    port = os.environ.get("S15_PORT", "8099")
    wt = os.environ["S15_WT"]
    codex_home = os.environ["S15_CODEX_HOME"]

    state = {"supervisor": procid.selfid("supervisor"), "lite": lite,
             "t0": time.time()}
    if lite:
        if os.environ.get("S15_OPEN_TTY") == "1":
            # Open a tty *slave* without O_NOCTTY. On a session leader with no
            # controlling terminal this is the classic accidental acquisition
            # the daemon double fork exists to prevent; on a non-leader it is
            # an ordinary open. Which of those marion's supervisor would be is
            # the whole difference between the two setsid variants, so it is
            # measured here rather than argued from the man page.
            import pty
            master, slave = pty.openpty()
            name = os.ttyname(slave)
            os.close(slave)
            fd = os.open(name, os.O_RDWR)
            # `master` and `fd` are deliberately never closed: the terminal has
            # to still exist when the observer reads `ps`.
            state["opened_tty"] = {"name": name, "master_fd": master, "fd": fd}
        state["status"] = "ready"
        write_atomic(state_path, state)
        while True:
            time.sleep(3600)

    # A child of the supervisor that was NOT given its own process group, so it
    # sits in the supervisor's group. Two jobs at once: it is the positive
    # control for any signal aimed at that group (if it survives, the signal
    # never landed and no "DEAD" reading in the run can be trusted), and it is
    # the shape S7 recorded of a real descendant that stays in the launching
    # process's group -- codex's `git fetch` for plugin sync did exactly this.
    control = subprocess.Popen(["/bin/sleep", "900"], stdin=subprocess.DEVNULL,
                               stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL)
    state["control_in_supervisor_group"] = {
        "pid": control.pid, "pgid": os.getpgid(control.pid)}

    os.makedirs(codex_home, exist_ok=True)
    with open(os.path.join(codex_home, "config.toml"), "w") as fh:
        fh.write(CONFIG_TOML % {"port": port, "wt": wt})

    cenv = dict(os.environ)
    cenv.update({"CODEX_HOME": codex_home, "S15_DUMMY_KEY": "dummy"})
    codex = subprocess.Popen(
        ["codex", "exec", "--json", "--skip-git-repo-check", "-C", wt,
         "spawn a durable child"],
        env=cenv, stdin=subprocess.DEVNULL,
        stdout=open(os.path.join(wt, "codex.stream.jsonl"), "wb"),
        stderr=open(os.path.join(wt, "codex.stderr.txt"), "wb"),
        preexec_fn=lambda: os.setpgid(0, 0))

    a = b = None
    deadline = time.time() + 120
    while time.time() < deadline:
        a = a or read_pidfile(os.environ["S15_PIDFILE"])
        b = b or read_pidfile(os.environ["S15_RUNAWAY_PIDFILE"])
        if a and b:
            time.sleep(3)  # let case B's tool call settle mid-flight
            break
        if codex.poll() is not None:
            break
        time.sleep(0.4)

    state["codex"] = {"pid": codex.pid, "exited_early": codex.poll() is not None}
    try:
        state["codex"]["pgid"] = os.getpgid(codex.pid)
        state["codex"]["sid"] = os.getsid(codex.pid)
    except OSError:
        pass
    state["case_a_pids"] = a
    state["case_b_pids"] = b
    if not a or not b:
        state["status"] = "UNMEASURED -- tool call did not record both pidfiles"
        write_atomic(state_path, state)
        return

    # Enumerate while everything is alive. After the kill the descendants
    # reparent to pid 1 and no ancestry walk can find them again (S7).
    rows = procid.ps_rows()
    pids = procid.descendant_pids(rows, codex.pid)
    pgids = procid.pgids_of(rows, pids)
    own = os.getpgid(0)
    state["tree"] = {
        "descendant_pids": pids,
        "descendant_pgids": pgids,
        "supervisor_pgid": own,
        "supervisor_pgid_in_tree": own in pgids,
        "signal_targets": procid.signal_targets(pgids, own),
        "dropped_by_own_pgid_filter": [g for g in pgids if g == own],
    }
    state["ps_before"] = [r for r in rows if r["pid"] in set(
        pids + [codex.pid, os.getpid()] + list(a.values()) + list(b.values()))]
    state["status"] = "ready"
    write_atomic(state_path, state)

    # Wait for the observer's instruction.
    while True:
        if os.path.exists(cmd_path):
            cmd = open(cmd_path).read().strip()
            if cmd == "two_step":
                sent = []
                for g in state["tree"]["signal_targets"]:
                    try:
                        os.killpg(g, signal.SIGKILL)
                        sent.append({"pgid": g, "errno": None})
                    except OSError as e:
                        sent.append({"pgid": g, "errno": e.errno})
                write_atomic(acted_path, {
                    "did": "two_step", "sent": sent,
                    "supervisor_alive_after_own_kill": True,
                    "supervisor": procid.selfid("supervisor-after-kill"),
                    "t": time.time()})
                # Keep running: whether we are still here IS the measurement,
                # and the observer confirms it from outside.
                while True:
                    time.sleep(3600)
            elif cmd == "idle":
                write_atomic(acted_path, {"did": "idle", "t": time.time()})
                while True:
                    time.sleep(3600)
        time.sleep(0.2)


if __name__ == "__main__":
    main()
