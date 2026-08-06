#!/usr/bin/env python3
"""S15 observer -- design doc §11 item 25.

Runs two things.

**Identity.** For each candidate detach mechanism, the supervisor's pid, ppid,
`getsid` and `getpgid` relative to (a) the launcher that started it and (b) the
children it starts.

**The decisive signal.** S7's scenario re-run from a *detached* supervisor: a
real `codex exec` mid-tool-call, against the canned provider, with a live
tool-call child and grandchild. Two experiments over that tree:

  tree_wide  one `killpg` on the supervisor's own process group, issued from
             outside -- literally "one signal to one group", the reading
             §7.3.2 says the document does not license. Measures what it
             reaches, and whether it reaches the supervisor itself.
  two_step   §6.7's per-node kill performed *by* the detached supervisor using
             run.rs's own algorithm, including the `signal_targets` filter that
             refuses to signal the supervisor's own group. Measures whether the
             supervisor survives its own tree kill, and what the filter costs.

The observer is deliberately not the supervisor's parent (nothing is, after it
detaches), so it can never `waitpid`; every liveness answer comes from
three-valued `kill(pid,0)` plus a pid-reuse guard (`procid.py`).

No model is called and no API key is used.
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
PORT = "8099"
REPORT = os.getenv("S15_REPORT", os.path.join(HERE, "s15-report.json"))
MECHANISMS = ["setsid_leader", "setsid_double", "inherit"]


def paths(tag):
    d = os.path.join(RUN, tag)
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(os.path.join(d, "wt"), exist_ok=True)
    return {"dir": d, "wt": os.path.join(d, "wt"),
            "state": os.path.join(d, "state.json"),
            "launcher": os.path.join(d, "launcher.json"),
            "cmd": os.path.join(d, "cmd"), "acted": os.path.join(d, "acted.json"),
            "pidfile": os.path.join(d, "wt", "pids-orphan.txt"),
            "runaway": os.path.join(d, "wt", "pids-runaway.txt"),
            "codex_home": os.path.join(d, "codex-home")}


def child_env(p, lite):
    env = dict(os.environ)
    env.update({
        "S15_STATE": p["state"], "S15_LAUNCHER": p["launcher"],
        "S15_CMD": p["cmd"], "S15_ACTED": p["acted"], "S15_WT": p["wt"],
        "S15_CODEX_HOME": p["codex_home"], "S15_PORT": PORT,
        "S15_PIDFILE": p["pidfile"], "S15_RUNAWAY_PIDFILE": p["runaway"],
        "S15_LITE": "1" if lite else "0",
    })
    return env


def start_provider(p):
    env = dict(os.environ)
    env.update({"S15_PORT": PORT,
                "S15_SPAWN_SH": os.path.join(HERE, "spawn.sh"),
                "S15_PIDFILE": p["pidfile"],
                "S15_RUNAWAY_SH": os.path.join(HERE, "runaway.sh"),
                "S15_RUNAWAY_PIDFILE": p["runaway"],
                "S15_REQLOG": os.path.join(p["dir"], "requests.jsonl"),
                "S15_HOLD_SECS": "240"})
    pr = subprocess.Popen([sys.executable, os.path.join(HERE, "canned_provider.py")],
                          env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    time.sleep(1.5)
    if pr.poll() is not None:
        sys.exit("provider died: " + pr.stderr.read().decode())
    return pr


def await_state(p, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if os.path.exists(p["state"]):
            try:
                s = json.load(open(p["state"]))
            except (ValueError, OSError):
                s = None
            if s and s.get("status"):
                return s
        time.sleep(0.3)
    return None


def kill_all(pids):
    for pid in pids:
        try:
            os.kill(int(pid), signal.SIGKILL)
        except OSError:
            pass


# --- identity pass --------------------------------------------------------

def identity_run(mech):
    """Launch a lite supervisor under `mech` and record who everyone is."""
    p = paths("id-" + mech)
    out = {"mechanism": mech, "observer": procid.selfid("observer")}
    subprocess.run([sys.executable, os.path.join(HERE, "launch.py"), mech],
                   env=child_env(p, lite=True), check=True)
    state = await_state(p, 20)
    if not state:
        out["status"] = "UNMEASURED -- supervisor never reported"
        return out
    out["launcher"] = json.load(open(p["launcher"]))
    out["supervisor"] = state["supervisor"]
    sup = state["supervisor"]["pid"]
    extra = [out["launcher"].get("sibling_job_pid")]
    watch = [sup] + [x for x in extra if x]
    out["ps"] = procid.ps_rows(watch + [os.getpid()])
    sup_row = next((r for r in out["ps"] if r["pid"] == sup), None)
    if sup_row is None:
        out["status"] = "UNMEASURED -- supervisor not visible in ps"
        return out
    out["relations"] = {
        "supervisor_ppid": sup_row["ppid"],
        "supervisor_tty": sup_row["tty"],
        "supervisor_is_session_leader": state["supervisor"]["sid"] == sup,
        "supervisor_is_group_leader": state["supervisor"]["pgid"] == sup,
        "shares_session_with_observer":
            state["supervisor"]["sid"] == os.getsid(0),
        "shares_pgroup_with_observer":
            state["supervisor"]["pgid"] == os.getpgid(0),
        "shares_pgroup_with_launcher":
            state["supervisor"]["pgid"] == out["launcher"]["launcher"]["pgid"],
        "shares_session_with_launcher":
            state["supervisor"]["sid"] == out["launcher"]["launcher"]["sid"],
        "supervisor_group_members": [
            r["pid"] for r in procid.ps_rows()
            if r["pgid"] == state["supervisor"]["pgid"]],
    }
    out["status"] = "ok"
    kill_all(watch)
    return out


# --- the codex scenario ---------------------------------------------------

def codex_run(mech, experiment):
    tag = "%s-%s" % (mech, experiment)
    p = paths(tag)
    out = {"mechanism": mech, "experiment": experiment,
           "observer": procid.selfid("observer")}
    provider = start_provider(p)
    # Negative control: unrelated to every target group. If this dies the
    # signal over-reached and the run says so.
    neg = subprocess.Popen(["/bin/sleep", "900"], stdin=subprocess.DEVNULL,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    watch = {}
    try:
        subprocess.run([sys.executable, os.path.join(HERE, "launch.py"), mech],
                       env=child_env(p, lite=False), check=True)
        state = await_state(p, 180)
        if not state:
            out["status"] = "UNMEASURED -- supervisor never reported"
            return out
        out["launcher"] = json.load(open(p["launcher"]))
        out["supervisor"] = state["supervisor"]
        out["codex"] = state.get("codex")
        out["tree"] = state.get("tree")
        out["ps_before"] = state.get("ps_before")
        if state["status"] != "ready":
            out["status"] = state["status"]
            return out

        sup = state["supervisor"]["pid"]
        sup_pgid = state["supervisor"]["pgid"]
        watch = {"supervisor": sup,
                 "codex": state["codex"]["pid"],
                 "control_in_supervisor_group":
                     state["control_in_supervisor_group"]["pid"],
                 "negative_control_outside_all_groups": neg.pid}
        for k, v in (state.get("case_a_pids") or {}).items():
            watch["A." + k] = v
        for k, v in (state.get("case_b_pids") or {}).items():
            watch["B." + k] = v
        if out["launcher"].get("sibling_job_pid"):
            watch["launcher_sibling_job"] = out["launcher"]["sibling_job_pid"]

        # --- false-pass guard 1: everything must be alive at enumeration.
        # Identity is taken first so the same ps sweep can rule out a zombie
        # masquerading as ALIVE before the signal is even sent.
        lstart = procid.identify(watch.values())
        raw = procid.verify(list(watch.values()), lstart)
        before = {k: raw[str(v)] for k, v in watch.items()}
        out["state_before"] = before
        out["watch"] = watch
        # Case A's pids legitimately die on their own the moment the tool call
        # completes -- S7 measured that -- so they are excluded from the
        # "must be alive" set rather than allowed to fake a kill's success.
        required = {k: v for k, v in watch.items() if not k.startswith("A.")}
        not_alive = [k for k in required if before[k] != procid.ALIVE]
        if not_alive:
            out["status"] = "UNMEASURED -- not alive at enumeration: %s" % not_alive
            return out
        # --- false-pass guard 2: identity, against pid reuse.
        out["lstart_before"] = {str(k): v for k, v in lstart.items()}

        # --- the signal.
        if experiment == "tree_wide":
            # Refuse to fire at a group that contains the observer: that would
            # kill the measurement and the answer would be unrecoverable.
            members = [r["pid"] for r in procid.ps_rows() if r["pgid"] == sup_pgid]
            out["target_group"] = {"pgid": sup_pgid, "members": members}
            if sup_pgid <= 1 or os.getpid() in members or sup_pgid == os.getpgid(0):
                out["status"] = "REFUSED -- target group contains the observer"
                return out
            os.killpg(sup_pgid, signal.SIGKILL)
            out["signal"] = "killpg(%d, SIGKILL) -- one signal, one group" % sup_pgid
        elif experiment == "two_step":
            with open(p["cmd"], "w") as fh:
                fh.write("two_step")
            deadline = time.time() + 20
            while time.time() < deadline and not os.path.exists(p["acted"]):
                time.sleep(0.2)
            out["acted"] = (json.load(open(p["acted"]))
                            if os.path.exists(p["acted"]) else None)
            if not out["acted"]:
                out["status"] = "UNMEASURED -- supervisor never acted"
                return out
            out["signal"] = ("supervisor ran run.rs's two-step kill: "
                             "killpg each of %s" % out["tree"]["signal_targets"])
        time.sleep(3.0)

        after = procid.verify(list(watch.values()), lstart)
        out["state_after"] = {k: after[str(v)] for k, v in watch.items()}

        # --- false-pass guards 3 and 4: the two controls.
        pos = out["state_after"]["control_in_supervisor_group"]
        negs = out["state_after"]["negative_control_outside_all_groups"]
        out["controls"] = {
            "positive_control_in_supervisor_group": pos,
            "negative_control_outside_all_groups": negs,
        }
        if experiment == "tree_wide" and pos not in (procid.DEAD, procid.ZOMBIE):
            out["status"] = ("DISCARDED -- positive control in the target group "
                             "did not die (%s); the signal never landed" % pos)
            return out
        if negs != procid.ALIVE:
            out["status"] = ("DISCARDED -- negative control outside every target "
                             "group is %s; the signal over-reached" % negs)
            return out

        out["supervisor_after"] = out["state_after"]["supervisor"]
        out["survivors"] = sorted(k for k, v in out["state_after"].items()
                                  if v == procid.ALIVE
                                  and not k.endswith("control_outside_all_groups"))
        out["zombies"] = sorted(k for k, v in out["state_after"].items()
                                if v == procid.ZOMBIE)
        out["unknown"] = sorted(k for k, v in out["state_after"].items()
                                if v == procid.UNKNOWN)
        out["status"] = "ok"
        return out
    finally:
        provider.terminate()
        kill_all(list(watch.values()) + [neg.pid])


def main():
    report = {"t0": time.time(), "host": os.uname().sysname + " " +
              os.uname().release, "codex_version": subprocess.run(
                  ["codex", "--version"], capture_output=True,
                  text=True).stdout.strip()}
    report["identity"] = [identity_run(m) for m in MECHANISMS]
    report["signal"] = []
    for mech in MECHANISMS:
        for exp in ("tree_wide", "two_step"):
            report["signal"].append(codex_run(mech, exp))
    return report


if __name__ == "__main__":
    r = main()
    with open(REPORT, "w") as fh:
        json.dump(r, fh, indent=2)
    print(json.dumps(r, indent=2))
