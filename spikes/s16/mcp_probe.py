#!/usr/bin/env python3
"""S16's probe: a minimal MCP stdio server that REFUSES to die quietly.

marion's `marion-supervisor mcp` bridge is a stdio subprocess started by the
*harness* (Claude Code), not by marion, and marion is about to background a
child inside it. So the question is what a harness exit does to that process
and to anything it started. This probe is the bridge, reduced to the parts
that make the answer observable:

  * it speaks enough MCP to be **connected** -- `initialize`, `notifications/*`,
    `tools/list` -- cribbed frame-for-frame from `crates/marion-supervisor/src/
    bridge.rs` (newline-delimited JSON, no Content-Length, protocolVersion
    "2024-11-05", `{"tools":{}}` capabilities). If the harness never gets a
    `tools/list` reply out of this process, the run measured nothing and the
    report must say so rather than report the silence as a finding.
  * it installs handlers for SIGTERM/SIGHUP/SIGINT that **log and keep
    running**. Exiting on the first signal would destroy the evidence for
    "is there a grace period before a harder one".
  * it logs stdin EOF and **keeps running**. A bridge that exits on EOF cannot
    tell you whether the harness would also have signalled it.
  * it heartbeats every 250 ms recording its own **ppid**, so reparenting to
    pid 1 is a thing you read off the log rather than infer.
  * it starts a **grandchild** (`grandchild.py`) with its own log, which is the
    thing marion's backgrounded child actually is.

SIGKILL cannot be logged. It is measured by its shape: heartbeats stop at a
timestamp with no preceding `signal` record and no `exit` record. That is only
readable because the probe otherwise never stops before its own deadline --
S7's lesson is that a leak check which cannot see a survivor is worse than
none, and its dual is that a probe which exits on its own looks exactly like a
probe that was killed.

Everything is appended and flushed line by line: a record that is still in a
buffer when SIGKILL lands is a record that never existed.

Env:
  S16_LOG          path to append JSONL to (required)
  S16_LIFE_SECS    self-terminate after this many seconds (default 90). The
                   probe is deliberately mortal so the spike cannot leak.
  S16_GRANDCHILD   path to grandchild.py (default: alongside this file)
  S16_GC_LOG       grandchild's own log path (default: S16_LOG + ".gc")
  S16_TAG          free-form label recorded in the `start` record
"""
import atexit
import json
import os
import signal
import subprocess
import sys
import threading
import time

LOG = os.environ["S16_LOG"]
LIFE = float(os.getenv("S16_LIFE_SECS", "90"))
HERE = os.path.dirname(os.path.abspath(__file__))
GRANDCHILD = os.getenv("S16_GRANDCHILD", os.path.join(HERE, "grandchild.py"))
GC_LOG = os.getenv("S16_GC_LOG", LOG + ".gc")
TAG = os.getenv("S16_TAG", "")

PROTOCOL_VERSION = "2024-11-05"   # bridge.rs: reply with a version we support
_lock = threading.Lock()


def rec(event, **kw):
    """Append one record and fsync it out of our hands.

    Ordering is the evidence, so every record carries the monotonic clock
    (system-wide on macOS, hence comparable with the runner's and the
    grandchild's) as well as wall time for correlating with `ps`.
    """
    row = {"t": round(time.monotonic(), 6), "wall": round(time.time(), 6),
           "event": event}
    row.update(kw)
    line = json.dumps(row) + "\n"
    with _lock:
        with open(LOG, "a") as fh:
            fh.write(line)
            fh.flush()
            os.fsync(fh.fileno())


# -- signals ---------------------------------------------------------------
# Log and CARRY ON. The point of the probe is to still be here when the next,
# harder signal arrives -- if one does.
def on_signal(sig, _frame):
    try:
        name = signal.Signals(sig).name
    except ValueError:
        name = "SIG%d" % sig
    rec("signal", sig=int(sig), name=name, ppid=os.getppid())


def install_handlers():
    """A handler on EVERY catchable signal, so death BY elimination is sound.

    SIGKILL cannot be caught and cannot be logged. The only way to turn "it
    died and said nothing" into "it was SIGKILLed" is to have arranged that
    every *other* way it could have died would have left a record. So: a
    logging handler on every signal Python will accept one for, an `atexit`
    record for any orderly interpreter shutdown, and an excepthook record for
    a crash. If the log ends on a `beat` with none of those three, the signal
    was uncatchable.
    """
    installed, refused = [], []
    for s in signal.Signals:
        if s in (signal.SIGKILL, signal.SIGSTOP):
            continue            # uncatchable by definition -- that is the point
        if s == signal.SIGPIPE:
            # A closed stdout would otherwise kill us and read as SIGKILL.
            # Take the EPIPE instead; `reply` records the write failure.
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
    """An orderly interpreter shutdown -- which SIGKILL never produces."""
    try:
        rec("atexit", ppid=os.getppid())
    except Exception:
        pass


def _excepthook(kind, value, tb):
    rec("crash", kind=getattr(kind, "__name__", str(kind)), value=str(value)[:400])


sys.excepthook = _excepthook
threading.excepthook = lambda a: rec(
    "thread_crash", kind=getattr(a.exc_type, "__name__", ""), value=str(a.exc_value)[:400])


# -- MCP over stdio --------------------------------------------------------
def reply(obj):
    """One compact line, flushed, exactly as `run_bridge` writes them."""
    try:
        sys.stdout.write(json.dumps(obj) + "\n")
        sys.stdout.flush()
        return True
    except (BrokenPipeError, OSError) as e:
        rec("stdout_write_failed", err=repr(e))
        return False


def tools():
    return [{"name": "noop",
             "description": "does nothing; exists so tools/list is non-empty",
             "inputSchema": {"type": "object", "properties": {}}}]


def serve_stdin():
    """Read lines until EOF, then log the EOF and RETURN -- the process lives on."""
    while True:
        line = sys.stdin.readline()
        if line == "":
            rec("stdin_eof", ppid=os.getppid())
            return
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except Exception:
            rec("stdin_unparsed", n=len(line))
            continue
        method, mid = msg.get("method"), msg.get("id")
        rec("rpc_in", method=method, has_id=mid is not None)
        if method == "initialize" and mid is not None:
            reply({"jsonrpc": "2.0", "id": mid, "result": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "s16-probe", "version": "1"}}})
            rec("rpc_out", method="initialize")
        elif method == "tools/list" and mid is not None:
            reply({"jsonrpc": "2.0", "id": mid,
                   "result": {"tools": tools()}})
            rec("rpc_out", method="tools/list")
        elif method == "tools/call" and mid is not None:
            reply({"jsonrpc": "2.0", "id": mid, "result": {
                "content": [{"type": "text", "text": "noop"}], "isError": False}})
            rec("rpc_out", method="tools/call")
        elif method and method.startswith("notifications/"):
            pass                       # no reply, bridge.rs does the same
        elif mid is not None:
            reply({"jsonrpc": "2.0", "id": mid,
                   "error": {"code": -32601, "message": "no method %s" % method}})
            rec("rpc_out", method="error")


# -- heartbeat -------------------------------------------------------------
def heartbeat(deadline):
    """Every 250 ms, our own ppid. Reparenting to pid 1 becomes a reading."""
    n = 0
    while time.monotonic() < deadline:
        time.sleep(0.25)
        n += 1
        rec("beat", n=n, ppid=os.getppid())


def main():
    started = time.monotonic()
    deadline = started + LIFE
    gc = subprocess.Popen(
        [sys.executable, GRANDCHILD],
        env=dict(os.environ, S16_LOG=GC_LOG, S16_LIFE_SECS=str(LIFE)),
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL)
    rec("start", tag=TAG, pid=os.getpid(), ppid=os.getppid(),
        pgid=os.getpgid(0), sid=os.getsid(0), grandchild_pid=gc.pid,
        life_secs=LIFE, argv=sys.argv, gc_log=GC_LOG,
        handlers=HANDLERS, handlers_refused=REFUSED)

    hb = threading.Thread(target=heartbeat, args=(deadline,), daemon=True)
    hb.start()
    t = threading.Thread(target=serve_stdin, daemon=True)
    t.start()

    while time.monotonic() < deadline:
        time.sleep(0.25)
    rec("deadline_exit", ppid=os.getppid(), life_secs=LIFE)
    # Do not orphan the grandchild past our own deadline: this spike must not
    # leak, and the grandchild has the same deadline anyway.
    try:
        gc.terminate()
    except Exception:
        pass
    os._exit(0)


if __name__ == "__main__":
    main()
