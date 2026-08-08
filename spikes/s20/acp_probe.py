#!/usr/bin/env python3
"""Probe an ACP agent: spawn, send `initialize`, print the raw response.

Usage: acp_probe.py <label> -- <argv...>
"""
import json, os, subprocess, sys, threading, time

label = sys.argv[1]
argv = sys.argv[sys.argv.index("--") + 1:]

cwd = os.environ.get("PROBE_CWD", "/tmp")
p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                     stderr=subprocess.PIPE, cwd=cwd, text=True, bufsize=1)

lines = []
errs = []

def reader(f, sink):
    for ln in f:
        sink.append(ln.rstrip("\n"))

t1 = threading.Thread(target=reader, args=(p.stdout, lines), daemon=True)
t2 = threading.Thread(target=reader, args=(p.stderr, errs), daemon=True)
t1.start(); t2.start()

req = {
    "jsonrpc": "2.0", "id": 0, "method": "initialize",
    "params": {
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": {"readTextFile": True, "writeTextFile": True},
            "terminal": True,
        },
    },
}
try:
    p.stdin.write(json.dumps(req) + "\n")
    p.stdin.flush()
except Exception as e:
    print(f"[{label}] WRITE FAILED: {e}")

deadline = time.time() + float(os.environ.get("PROBE_TIMEOUT", "25"))
while time.time() < deadline and not lines:
    time.sleep(0.2)
    if p.poll() is not None and not lines:
        time.sleep(0.5)
        break

out = os.environ.get("PROBE_OUT")
if out and lines:
    with open(out, "w") as f:
        f.write(lines[0] + "\n")

print(f"[{label}] argv={argv}")
print(f"[{label}] exit={p.poll()}")
print(f"[{label}] stdout lines ({len(lines)}):")
for ln in lines[:10]:
    print("   ", ln[:2000])
print(f"[{label}] stderr lines ({len(errs)}):")
for ln in errs[:15]:
    print("   ", ln[:600])

try:
    p.kill()
except Exception:
    pass
