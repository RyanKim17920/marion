#!/usr/bin/env python3
"""S16 -- what a harness exit does to its MCP stdio server, and to that
server's grandchild.

    python3 spikes/s16/run_probe.py harness            OUTDIR [PORT]
    python3 spikes/s16/run_probe.py harness-own-pgroup OUTDIR [PORT]
    python3 spikes/s16/run_probe.py control            OUTDIR

`harness-own-pgroup` is `harness` with `claude` put in a process group of its
own, which is marion's arrangement and which is what makes "the harness did not
use killpg" a measurement rather than an assumption (see `run_harness`).

`harness` runs a real headless `claude` against the canned provider with
`spikes/s16/mcp_probe.py` registered as a `--mcp-config` stdio server, waits
for `claude` to exit, and then WATCHES -- ps every 250 ms, the probe's and the
grandchild's own logs -- for S16_OBSERVE_SECS.

`control` is the negative control: the same probe, the same grandchild, no
harness at all. The runner closes the probe's stdin itself at a known moment.
It exists because S7's lesson is that a leak check which cannot see a survivor
is worse than none: a probe whose logging is broken records silence, and
silence looks exactly like "the harness killed it instantly". The control
proves the log can record an EOF, a survival, and a reparent.

What the run can and cannot conclude:

  * a `signal` record IS the answer to "does it signal";
  * NO `signal` record plus `rpc_in` records for `initialize` and `tools/list`
    IS the answer to "it merely closes stdin" -- but ONLY if `tools/list` was
    answered, because a server the harness never connected to is a server the
    harness has no teardown for. If `tools/list` never arrived the run is
    reported UNMEASURED, not "no signal".
  * heartbeats that stop with no `signal` and no `deadline_exit` record are
    the signature of SIGKILL. It is a signature, not a log line, and the
    report names it as such.

Costs nothing: `ANTHROPIC_BASE_URL` points at 127.0.0.1, `ANTHROPIC_API_KEY`
is a literal non-credential string, and no model is ever called.
"""
import json
import os
import shutil
import signal
import subprocess
import sys
import threading
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "s15"))
import procid  # noqa: E402  -- S15's liveness/pid-reuse primitives, reused whole

PROBE = os.path.join(HERE, "mcp_probe.py")
PROVIDER = os.path.join(HERE, "canned_provider.py")
OBSERVE = float(os.getenv("S16_OBSERVE_SECS", "15"))
LIFE = float(os.getenv("S16_LIFE_SECS", "90"))
CLAUDE = os.getenv("S16_CLAUDE", "claude")

#: The credential-shaped env var must be set to something or the CLI refuses to
#: start; this literal is not a credential and never leaves 127.0.0.1.
FAKE_TOKEN = "s16-not-a-credential"


def now():
    return round(time.monotonic(), 6)


def read_log(path):
    rows = []
    if not os.path.exists(path):
        return rows
    for line in open(path):
        line = line.strip()
        if not line:
            continue
        try:
            rows.append(json.loads(line))
        except Exception:
            rows.append({"event": "_unparsed", "raw": line[:200]})
    return rows


def first(rows, **match):
    for r in rows:
        if all(r.get(k) == v for k, v in match.items()):
            return r
    return None


def write(path, obj):
    tmp = path + ".tmp"
    with open(tmp, "w") as fh:
        json.dump(obj, fh, indent=2)
    os.replace(tmp, path)


def versions():
    def cap(argv):
        try:
            p = subprocess.run(argv, capture_output=True, text=True, timeout=20)
            return (p.stdout + p.stderr).strip()[:200]
        except Exception as e:
            return "unavailable: %r" % (e,)
    return {"claude": cap([CLAUDE, "--version"]),
            "claude_path": shutil.which(CLAUDE) or CLAUDE,
            "python": sys.version.split()[0],
            "uname": cap(["uname", "-srm"])}


def wait_for_start(logpath, timeout=20):
    """Block until the probe's `start` record exists; it carries the pids."""
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        r = first(read_log(logpath), event="start")
        if r:
            return r
        time.sleep(0.1)
    return None


def observe(pids, seconds, ident, out=None, period=0.25, stop=None):
    """ps every `period` seconds, appending to `out` if given.

    Runs both inline (the post-exit watch) and on a thread from the moment the
    pids are known (the *during*-teardown watch). The threaded one matters:
    while the runner is blocked in `communicate()` the harness does its whole
    teardown, and a report that only looks afterwards can say what survived but
    not in what ORDER things died.
    """
    samples = [] if out is None else out
    end = time.monotonic() + seconds
    while time.monotonic() < end and not (stop and stop.is_set()):
        rows = procid.ps_rows(pids)
        samples.append({"t": now(),
                        "state": procid.verify(pids, ident),
                        "rows": rows})
        time.sleep(period)
    return samples


def transitions(samples, pid):
    """The state changes for one pid, as (t, state) -- ordering, not elapsed
    time, is what this spike is allowed to assert on."""
    out, last, prev_t = [], None, None
    for s in samples:
        st = s["state"].get(str(pid))
        if st != last:
            out.append({"t": s["t"], "state": st, "prev_sample_t": prev_t})
            last = st
        prev_t = s["t"]
    return out


# -- the harness case ------------------------------------------------------
def run_harness(out, port, own_pgroup=False):
    """`own_pgroup` puts `claude` in a process group of its own, which is what
    marion does (`setpgid` at spawn, §9 / S7 / S15).

    It is not a detail. Without it the harness shares the *runner's* group, so
    a group-scoped teardown would have killed the runner too -- meaning "the
    harness did not use killpg" would be unfalsifiable rather than measured.
    With it, the harness IS its group's leader and its MCP server and that
    server's grandchild are the only other members, so killpg is available to
    it and a grandchild that survives is a grandchild the harness chose not to
    reach."""
    os.makedirs(out, exist_ok=True)
    log = os.path.join(out, "probe.jsonl")
    gclog = os.path.join(out, "grandchild.jsonl")
    cfg = os.path.join(out, "mcp-config.json")
    wt = os.path.join(out, "wt")
    os.makedirs(wt, exist_ok=True)
    for p in (log, gclog):
        open(p, "w").close()

    json_cfg = {"mcpServers": {"s16probe": {
        "type": "stdio",
        "command": sys.executable,
        "args": [PROBE],
        "env": {"S16_LOG": log, "S16_GC_LOG": gclog,
                "S16_LIFE_SECS": str(LIFE), "S16_TAG": "harness"}}}}
    write(cfg, json_cfg)

    prov = subprocess.Popen(
        [sys.executable, PROVIDER],
        env=dict(os.environ, S16_PORT=str(port),
                 S16_REQLOG=os.path.join(out, "requests.jsonl")),
        stdout=open(os.path.join(out, "provider.log"), "w"),
        stderr=subprocess.STDOUT)
    for _ in range(20):
        try:
            urllib.request.urlopen("http://127.0.0.1:%d/" % port, timeout=1).read()
            break
        except Exception:
            time.sleep(0.3)

    # marion's own claude argv, minus the marion-specific tool names.
    # `crates/marion-harness/src/adapter.rs` pins the shape.
    argv = [CLAUDE, "-p",
            "--output-format", "stream-json",
            "--input-format", "stream-json",
            "--verbose",
            "--allowedTools", "mcp__s16probe__noop",
            "--strict-mcp-config",
            "--mcp-config", cfg,
            "--setting-sources", "",
            "--model", "haiku"]
    env = dict(os.environ,
               ANTHROPIC_BASE_URL="http://127.0.0.1:%d" % port,
               ANTHROPIC_AUTH_TOKEN=FAKE_TOKEN,
               ANTHROPIC_API_KEY="",
               CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1")

    t_spawn = now()
    kw = {"process_group": 0} if own_pgroup else {}
    child = subprocess.Popen(argv, cwd=wt, env=env,
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, text=True, **kw)
    child.stdin.write(json.dumps({
        "type": "user",
        "message": {"role": "user", "content": [
            {"type": "text", "text": "Reply with exactly: S16-OK"}]}}) + "\n")
    child.stdin.flush()
    child.stdin.close()

    start = wait_for_start(log, timeout=30)
    probe_pid = (start or {}).get("pid")
    gc_pid = (start or {}).get("grandchild_pid")
    pids = [p for p in (probe_pid, gc_pid) if p]
    ident = procid.identify(pids) if pids else {}

    # Snapshot the whole family WHILE the harness is alive: this is the only
    # moment at which "same process group / same session as the harness" is a
    # readable fact rather than a reconstruction.
    live_rows = procid.ps_rows([child.pid] + pids) if pids else \
        procid.ps_rows([child.pid])

    # Watch from OUTSIDE, at 100 ms, across the teardown itself -- the runner is
    # about to block in communicate() and the whole event happens in there.
    watch, watch_pids = [], []
    stop = threading.Event()
    if pids:
        watch_pids = pids + [child.pid]
        watch_ident = procid.identify(watch_pids)
        threading.Thread(
            target=observe,
            kwargs={"pids": watch_pids, "seconds": 600.0, "ident": watch_ident,
                    "out": watch, "period": 0.1, "stop": stop},
            daemon=True).start()

    stdout, stderr = child.communicate(timeout=180)
    t_exit = now()
    rc = child.returncode

    samples = observe(pids, OBSERVE, ident) if pids else []
    stop.set()

    probe_rows = read_log(log)
    gc_rows = read_log(gclog)
    report = {
        "case": "harness-own-pgroup" if own_pgroup else "harness",
        "harness_own_process_group": own_pgroup,
        "versions": versions(),
        "argv": argv[1:],
        "env_overrides": sorted(["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN",
                                 "ANTHROPIC_API_KEY",
                                 "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"]),
        "mcp_config": json_cfg,
        "runner": procid.selfid("runner"),
        "harness": {"pid": child.pid, "t_spawn": t_spawn, "t_exit": t_exit,
                    "returncode": rc,
                    "stdout_lines": len(stdout.splitlines()),
                    "stderr": stderr[-4000:]},
        "family_while_alive": live_rows,
        "probe_start": start,
        "observe_secs": OBSERVE,
        "samples": samples,
        "watch_transitions": {str(p): transitions(watch, p) for p in watch_pids},
        "watch_period_secs": 0.1,
        "watch_samples": len(watch),
        "probe_log": probe_rows,
        "grandchild_log": gc_rows,
    }
    report["answers"] = derive(report)

    for pid in pids:
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
    prov.terminate()
    try:
        prov.wait(timeout=5)
    except Exception:
        prov.kill()
    report["cleanup"] = {"killed": pids,
                         "after": procid.verify(pids, ident) if pids else {}}
    return report


# -- the negative control --------------------------------------------------
def run_control(out):
    os.makedirs(out, exist_ok=True)
    log = os.path.join(out, "probe.jsonl")
    gclog = os.path.join(out, "grandchild.jsonl")
    for p in (log, gclog):
        open(p, "w").close()

    env = dict(os.environ, S16_LOG=log, S16_GC_LOG=gclog,
               S16_LIFE_SECS=str(LIFE), S16_TAG="control")
    t_spawn = now()
    p = subprocess.Popen([sys.executable, PROBE], env=env,
                         stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE)
    start = wait_for_start(log, timeout=30)
    pids = [x for x in ((start or {}).get("pid"),
                        (start or {}).get("grandchild_pid")) if x]
    ident = procid.identify(pids) if pids else {}

    # Speak the same MCP opening the harness would, so the control exercises the
    # same code path -- then close stdin, which is the thing under test.
    for msg in ({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                 "params": {"protocolVersion": "2025-06-18"}},
                {"jsonrpc": "2.0", "method": "notifications/initialized"},
                {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}):
        p.stdin.write((json.dumps(msg) + "\n").encode())
        p.stdin.flush()
    time.sleep(1.0)
    t_eof = now()
    p.stdin.close()

    samples = observe(pids, OBSERVE, ident) if pids else []
    report = {
        "case": "control",
        "versions": versions(),
        "runner": procid.selfid("runner"),
        "harness": None,
        "t_spawn": t_spawn, "t_stdin_closed": t_eof,
        "probe_start": start,
        "observe_secs": OBSERVE,
        "samples": samples,
        "probe_log": read_log(log),
        "grandchild_log": read_log(gclog),
    }
    report["answers"] = derive(report)
    for pid in pids:
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
    report["cleanup"] = {"killed": pids,
                         "after": procid.verify(pids, ident) if pids else {}}
    return report


def bracket(trans, period):
    """(last_alive_t, first_dead_t) from a transition list, or None."""
    for i, x in enumerate(trans):
        if x["state"] == "DEAD" and i > 0:
            return {"last_alive_t": x["prev_sample_t"], "first_dead_t": x["t"],
                    "resolution_secs": period}
    return None


# -- reading the logs ------------------------------------------------------
def derive(r):
    """Turn the records into the three answers, refusing to answer where the
    records do not support one."""
    probe, gc = r["probe_log"], r["grandchild_log"]
    connected = bool(first(probe, event="rpc_out", method="tools/list"))
    signals = [x for x in probe if x.get("event") == "signal"]
    gc_signals = [x for x in gc if x.get("event") == "signal"]
    eof = first(probe, event="stdin_eof")
    beats = [x for x in probe if x.get("event") == "beat"]
    gc_beats = [x for x in gc if x.get("event") == "beat"]
    exited = first(probe, event="deadline_exit")
    orderly = first(probe, event="atexit")
    crashed = first(probe, event="crash") or first(probe, event="thread_crash")

    t_exit = (r.get("harness") or {}).get("t_exit")
    ppids_after = sorted({x["ppid"] for x in beats
                          if t_exit is not None and x["t"] > t_exit})
    gc_ppids_after = sorted({x["ppid"] for x in gc_beats
                             if t_exit is not None and x["t"] > t_exit})

    # Last state each pid was seen in, with S15's pid-reuse guard already
    # applied by `verify` at sample time.
    last = r["samples"][-1]["state"] if r["samples"] else {}

    out = {
        "mcp_connected": connected,
        "signals_to_server": [{"sig": s["sig"], "name": s["name"], "t": s["t"]}
                              for s in signals],
        "signals_to_grandchild": [{"sig": s["sig"], "name": s["name"], "t": s["t"]}
                                  for s in gc_signals],
        "stdin_eof_seen": bool(eof),
        "stdin_eof_t": eof["t"] if eof else None,
        "harness_exit_t": t_exit,
        "server_beats": len(beats),
        "server_last_beat_t": beats[-1]["t"] if beats else None,
        "server_beats_after_harness_exit":
            len([x for x in beats if t_exit is not None and x["t"] > t_exit]),
        "server_ppids_after_harness_exit": ppids_after,
        "grandchild_beats": len(gc_beats),
        "grandchild_last_beat_t": gc_beats[-1]["t"] if gc_beats else None,
        "grandchild_beats_after_harness_exit":
            len([x for x in gc_beats if t_exit is not None and x["t"] > t_exit]),
        "grandchild_ppids_after_harness_exit": gc_ppids_after,
        "server_voluntary_exit": bool(exited),
        "server_orderly_shutdown": bool(orderly),
        "server_crashed": bool(crashed),
        "server_signal_handlers_installed":
            len((r.get("probe_start") or {}).get("handlers") or []),
        "server_signal_handlers_refused":
            (r.get("probe_start") or {}).get("handlers_refused"),
        "last_observed_state": last,
        # The 100 ms outside watcher brackets the server's death between the
        # last sample that saw it and the first that did not. Reported as a
        # bracket, never as a single instant: `ps` at 100 ms cannot say more.
        "server_death_bracket": bracket(
            r.get("watch_transitions", {}).get(
                str((r.get("probe_start") or {}).get("pid")), []),
            r.get("watch_period_secs")),
        "harness_death_bracket": bracket(
            r.get("watch_transitions", {}).get(
                str(((r.get("harness") or {}) or {}).get("pid")), []),
            r.get("watch_period_secs")),
    }

    if r["case"].startswith("harness") and not connected:
        out["verdict"] = ("UNMEASURED: the harness never obtained a tools/list "
                          "reply from the probe, so nothing was torn down and "
                          "the silence means nothing")
    elif r["case"].startswith("harness"):
        survived = out["server_beats_after_harness_exit"] > 0
        gc_survived = out["grandchild_beats_after_harness_exit"] > 0
        out["verdict"] = {
            "signalled": bool(signals),
            "server_survived_harness_exit": survived,
            "grandchild_survived_harness_exit": gc_survived,
            "server_reparented_to_init": ppids_after == [1],
            "grandchild_reparented_to_init": gc_ppids_after == [1],
            # SIGKILL leaves no record; it is read BY ELIMINATION. The probe
            # holds a logging handler on every catchable signal, an atexit
            # record for any orderly shutdown and an excepthook record for a
            # crash. So: it is gone, it did not reach its own deadline, it did
            # not shut down in an orderly way, it did not crash, and the last
            # thing in its log is not the signal it died of (it survived and
            # kept beating past every signal it DID log). Only an uncatchable
            # signal is left, and SIGSTOP would leave it alive.
            "died_without_a_record": (not exited) and (not orderly)
                                     and (not crashed) and not survived,
            "kept_beating_after_last_logged_signal": bool(
                signals and beats and beats[-1]["t"] > signals[-1]["t"]),
        }
    else:
        out["verdict"] = {
            "logging_works": len(beats) > 0 and len(gc_beats) > 0,
            "eof_recorded": bool(eof),
            "survives_own_eof": bool(eof) and any(
                x["t"] > eof["t"] for x in beats),
            "grandchild_alive_at_end": last.get(
                str((r.get("probe_start") or {}).get("grandchild_pid"))),
        }
    return out


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    case, out = sys.argv[1], os.path.abspath(sys.argv[2])
    port = int(sys.argv[3]) if len(sys.argv) > 3 else int(os.getenv("S16_PORT", "8116"))
    if case == "harness":
        r = run_harness(out, port)
    elif case == "harness-own-pgroup":
        r = run_harness(out, port, own_pgroup=True)
    elif case == "control":
        r = run_control(out)
    else:
        sys.exit("unknown case: " + case)
    path = os.getenv("S16_REPORT") or os.path.join(HERE, "s16-%s.json" % case)
    write(path, r)
    print(json.dumps(r["answers"], indent=2))
    print("wrote " + path)


if __name__ == "__main__":
    main()
