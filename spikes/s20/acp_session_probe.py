#!/usr/bin/env python3
"""How far does an ACP agent get without authentication?

`initialize` -> `session/new`. Records both answers verbatim. The point is to find
where the wall is, so `marion doctor --adapter` asserts only what it can actually reach.

Usage: acp_session_probe.py <label> -- <argv...>
"""
import json, os, subprocess, sys, threading, time

label = sys.argv[1]
argv = sys.argv[sys.argv.index("--") + 1:]
cwd = os.environ.get("PROBE_CWD", "/tmp/acpprobe")

p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                     stderr=subprocess.PIPE, cwd=cwd, text=True, bufsize=1)
lines, errs = [], []
for f, sink in ((p.stdout, lines), (p.stderr, errs)):
    threading.Thread(target=lambda f=f, s=sink: [s.append(l.rstrip("\n")) for l in f],
                     daemon=True).start()


def send(obj):
    p.stdin.write(json.dumps(obj) + "\n")
    p.stdin.flush()


def wait_for(n, secs=25):
    end = time.time() + secs
    while time.time() < end and len(lines) < n:
        time.sleep(0.2)
        if p.poll() is not None:
            time.sleep(0.5)
            break


send({"jsonrpc": "2.0", "id": 0, "method": "initialize",
      "params": {"protocolVersion": 1,
                 "clientCapabilities": {"fs": {"readTextFile": True, "writeTextFile": True},
                                        "terminal": True}}})
wait_for(1)
send({"jsonrpc": "2.0", "id": 1, "method": "session/new",
      "params": {"cwd": cwd, "mcpServers": []}})
wait_for(2)

print(json.dumps({"label": label, "argv": argv, "exit": p.poll(),
                  "stdout": lines, "stderr": errs[:20]}, indent=2))
try:
    p.kill()
except Exception:
    pass
