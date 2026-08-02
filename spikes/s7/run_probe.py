#!/usr/bin/env python3
"""S7 probe -- design doc §11 item 18.

Does `killpg` on the process group marion creates for `codex exec` reach the
subprocesses codex spawns for its tool calls?

Two cases are driven by one code-mode `exec` tool call:

  A "orphan"        -- exec_command runs a script that backgrounds two sleepers
                       and exits. The tool call COMPLETES; the sleepers are
                       orphaned runaways.
  B "still running" -- exec_command runs a script that stays in the foreground,
                       with yield_time_ms=250 so `exec` yields back to the model
                       while the command is STILL ALIVE. This is the shape of a
                       genuine runaway tool call at timeout expiry.

Procedure
  1. Start the canned provider (127.0.0.1:8098).
  2. Spawn `codex exec` with preexec_fn=os.setpgid(0, 0) -- exactly what
     marion's timeout enforcement does at spawn.
  3. Wait for both pidfiles; poll liveness continuously so we know whether a pid
     died on its own before the kill.
  4. Snapshot `ps -o pid,ppid,pgid,sess,command` for codex + every descendant.
  5. os.killpg(os.getpgid(codex_pid), SIGKILL).
  6. Probe every recorded pid with os.kill(pid, 0) and require ESRCH.

Writes a JSON report to S7_REPORT (default ./s7-report.json).
"""
import errno
import json
import os
import signal
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
WT = os.path.join(HERE, "wt")
PIDFILE = os.path.join(WT, "pids-orphan.txt")
RUNAWAY_PIDFILE = os.path.join(WT, "pids-runaway.txt")
SPAWN_SH = os.path.join(HERE, "spawn.sh")
RUNAWAY_SH = os.path.join(HERE, "runaway.sh")
REPORT = os.getenv("S7_REPORT", os.path.join(HERE, "s7-report.json"))
REQLOG = os.getenv("S7_REQLOG", os.path.join(HERE, "s7-requests.jsonl"))
PORT = "8098"
CODEX_HOME = os.path.join(HERE, "codex-home")

# Generated at runtime so nothing machine-specific is committed: the [projects.…]
# trust entry needs the absolute path of WT, which differs per checkout.
CONFIG_TOML = '''\
model = "gpt-5.6-sol"
model_provider = "canned"
approval_policy = "never"
sandbox_mode = "workspace-write"

[model_providers.canned]
name = "canned"
base_url = "http://127.0.0.1:%(port)s/v1"
wire_api = "responses"
env_key = "S7_DUMMY_KEY"

[projects."%(wt)s"]
trust_level = "trusted"
'''


def write_codex_config():
    """Write codex-home/config.toml with WT's absolute path resolved here."""
    os.makedirs(CODEX_HOME, exist_ok=True)
    path = os.path.join(CODEX_HOME, "config.toml")
    with open(path, "w") as fh:
        fh.write(CONFIG_TOML % {"port": PORT, "wt": WT})
    return path


def ps(pids):
    pids = [str(p) for p in sorted(set(pids))]
    if not pids:
        return []
    out = subprocess.run(
        ["ps", "-o", "pid=,ppid=,pgid=,sess=,command=", "-p", ",".join(pids)],
        capture_output=True, text=True).stdout
    rows = []
    for line in out.splitlines():
        f = line.split(None, 4)
        if len(f) < 5:
            continue
        pid = int(f[0])
        try:
            sid = os.getsid(pid)
        except OSError:
            sid = None
        rows.append({"pid": pid, "ppid": int(f[1]), "pgid": int(f[2]),
                     "sess_ps": int(f[3]), "sid": sid, "command": f[4][:160]})
    return rows


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except OSError as e:
        return e.errno != errno.ESRCH  # EPERM => exists


def descendants_of(pid):
    out = subprocess.run(["ps", "-e", "-o", "pid=,ppid=,pgid="],
                         capture_output=True, text=True).stdout
    kids, pg = {}, {}
    for line in out.splitlines():
        f = line.split()
        if len(f) < 3:
            continue
        p, pp, g = int(f[0]), int(f[1]), int(f[2])
        kids.setdefault(pp, []).append(p)
        pg.setdefault(g, []).append(p)
    seen, stack = set(), [pid]
    while stack:
        cur = stack.pop()
        if cur in seen:
            continue
        seen.add(cur)
        stack.extend(kids.get(cur, []))
    try:
        seen |= set(pg.get(os.getpgid(pid), []))
    except OSError:
        pass
    return sorted(seen)


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
    for path in (PIDFILE, RUNAWAY_PIDFILE, REPORT, REQLOG):
        if os.path.exists(path):
            os.remove(path)
    os.makedirs(WT, exist_ok=True)

    env = dict(os.environ)
    env.update({"S7_PORT": PORT, "S7_SPAWN_SH": SPAWN_SH, "S7_PIDFILE": PIDFILE,
                "S7_RUNAWAY_SH": RUNAWAY_SH, "S7_RUNAWAY_PIDFILE": RUNAWAY_PIDFILE,
                "S7_REQLOG": REQLOG, "S7_HOLD_SECS": "240"})
    provider = subprocess.Popen(
        [sys.executable, os.path.join(HERE, "canned_provider.py")], env=env,
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    time.sleep(1.5)
    if provider.poll() is not None:
        sys.exit("provider died: " + provider.stderr.read().decode())

    report = {"t0": time.time()}
    codex = None
    all_pids = {}
    try:
        report["config_toml"] = write_codex_config()
        cenv = dict(os.environ)
        cenv.update({"CODEX_HOME": CODEX_HOME, "S7_DUMMY_KEY": "dummy"})
        codex = subprocess.Popen(
            ["codex", "exec", "--json", "--skip-git-repo-check", "-C", WT,
             "spawn a durable child"],
            env=cenv, stdin=subprocess.DEVNULL,
            stdout=open(os.path.join(HERE, "s7-codex.stream.jsonl"), "wb"),
            stderr=open(os.path.join(HERE, "s7-codex.stderr.txt"), "wb"),
            preexec_fn=lambda: os.setpgid(0, 0))
        parent_pgid = os.getpgid(codex.pid)
        report.update({"codex_pid": codex.pid, "codex_pgid": parent_pgid,
                       "harness_pid": os.getpid(), "harness_pgid": os.getpgid(0)})

        # Wait for both pidfiles, polling liveness so self-deaths are visible.
        a = b = None
        first_seen = {}
        died_before_kill = {}
        deadline = time.time() + 120
        while time.time() < deadline:
            a = a or read_pidfile(PIDFILE)
            b = b or read_pidfile(RUNAWAY_PIDFILE)
            for label, d in (("A", a), ("B", b)):
                for name, pid in (d or {}).items():
                    key = "%s.%s" % (label, name)
                    if key not in first_seen:
                        first_seen[key] = pid
                    if key not in died_before_kill and not alive(pid):
                        died_before_kill[key] = round(time.time() - report["t0"], 2)
            if a and b:
                # settle: give case B's tool call a beat to be mid-flight
                time.sleep(3)
                break
            if codex.poll() is not None:
                break
            time.sleep(0.4)

        report["case_a_pids"] = a
        report["case_b_pids"] = b
        report["codex_exited_early"] = codex.poll() is not None
        if not a or not b:
            report["verdict"] = "UNMEASURED -- tool call did not record both pidfiles"
            return report

        all_pids = {("A." + k): v for k, v in a.items()}
        all_pids.update({("B." + k): v for k, v in b.items()})

        watch = sorted(set(list(all_pids.values()) + [codex.pid]
                           + descendants_of(codex.pid)))
        report["ps_before_kill"] = ps(watch)
        report["alive_before_kill"] = {k: alive(v) for k, v in all_pids.items()}
        report["died_on_their_own_before_kill_secs"] = died_before_kill
        report["harness_sid"] = os.getsid(0)
        report["codex_sid"] = os.getsid(codex.pid)
        # every distinct pgid under codex, captured BEFORE the kill -- after the
        # kill these processes reparent to pid 1 and are no longer discoverable
        # by ancestry.
        pgids = sorted({r["pgid"] for r in report["ps_before_kill"]})
        report["descendant_pgids_before_kill"] = pgids

        os.killpg(parent_pgid, signal.SIGKILL)
        report["killpg_target"] = parent_pgid
        time.sleep(2.5)

        report["alive_after_kill"] = {k: alive(v) for k, v in all_pids.items()}
        report["codex_alive_after_kill"] = alive(codex.pid) and codex.poll() is None
        report["ps_after_kill"] = ps(watch)

        live_before = {k for k, v in report["alive_before_kill"].items() if v}
        survivors = sorted(k for k in live_before if report["alive_after_kill"][k])
        report["live_at_kill_time"] = sorted(live_before)
        report["survivors"] = survivors
        if not live_before:
            report["verdict"] = "INCONCLUSIVE -- nothing was alive at kill time"
        elif survivors:
            report["verdict"] = "KILLPG LEAKS -- survivors: %s" % survivors
        else:
            report["verdict"] = "KILLPG REACHES all tool-call subprocesses alive at kill time"

        # Stage 3: does the remedy work? killpg every pgid we recorded under
        # codex before the kill, not just codex's own.
        if survivors:
            for g in pgids:
                try:
                    os.killpg(g, signal.SIGKILL)
                except OSError:
                    pass
            report["remedy_killpg_targets"] = pgids
            time.sleep(1.5)
            report["alive_after_remedy"] = {k: alive(v) for k, v in all_pids.items()}
            remaining = sorted(k for k in survivors if report["alive_after_remedy"][k])
            report["remedy_survivors"] = remaining
            report["remedy_verdict"] = ("recursive pgid sweep CLEANS UP" if not remaining
                                        else "recursive pgid sweep INSUFFICIENT: %s" % remaining)
        return report
    finally:
        try:
            if codex is not None:
                codex.wait(timeout=5)
        except Exception:
            pass
        provider.terminate()
        for pid in all_pids.values():   # never leave sleepers behind
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass


if __name__ == "__main__":
    r = main()
    with open(REPORT, "w") as fh:
        json.dump(r, fh, indent=2)
    print(json.dumps(r, indent=2))
