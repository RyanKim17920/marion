#!/usr/bin/env python3
"""S16's grandchild: what marion's backgrounded child looks like from outside.

Started by `mcp_probe.py`, which is itself started by the harness. It does
nothing but heartbeat its own ppid every 250 ms into its own log, so that
"did the grandchild outlive the harness" and "was it reparented" are two
separate readings rather than one inference.

It logs signals and keeps running, for the same reason the probe does: the
question is whether a *harder* signal follows, and a process that exits on the
first one cannot answer it.

Mortal by construction (S16_LIFE_SECS) so the spike cannot leak a survivor.
"""
import atexit
import json
import os
import signal
import time

LOG = os.environ["S16_LOG"]
LIFE = float(os.getenv("S16_LIFE_SECS", "90"))


def rec(event, **kw):
    row = {"t": round(time.monotonic(), 6), "wall": round(time.time(), 6),
           "event": event}
    row.update(kw)
    with open(LOG, "a") as fh:
        fh.write(json.dumps(row) + "\n")
        fh.flush()
        os.fsync(fh.fileno())


def on_signal(sig, _frame):
    try:
        name = signal.Signals(sig).name
    except ValueError:
        name = "SIG%d" % sig
    rec("signal", sig=int(sig), name=name, ppid=os.getppid())


def install_handlers():
    """Same elimination argument as the probe's: a handler on every catchable
    signal, so "the grandchild was never signalled" is a reading and not an
    absence of instrumentation."""
    installed, refused = [], []
    for s in signal.Signals:
        if s in (signal.SIGKILL, signal.SIGSTOP):
            continue
        if s == signal.SIGPIPE:
            signal.signal(s, signal.SIG_IGN)
            installed.append(s.name)
            continue
        try:
            signal.signal(s, on_signal)
            installed.append(s.name)
        except (OSError, ValueError, RuntimeError):
            refused.append(s.name)
    return installed, refused


HANDLERS, REFUSED = install_handlers()


@atexit.register
def _atexit():
    try:
        rec("atexit", ppid=os.getppid())
    except Exception:
        pass


def main():
    rec("start", pid=os.getpid(), ppid=os.getppid(), pgid=os.getpgid(0),
        sid=os.getsid(0), life_secs=LIFE,
        handlers=HANDLERS, handlers_refused=REFUSED)
    deadline = time.monotonic() + LIFE
    n = 0
    while time.monotonic() < deadline:
        time.sleep(0.25)
        n += 1
        rec("beat", n=n, ppid=os.getppid())
    rec("deadline_exit", ppid=os.getppid())


if __name__ == "__main__":
    main()
