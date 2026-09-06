#!/usr/bin/env python3
"""S27 step 0: discover the on-disk provider format by letting `cline auth` write it.

    auth_probe.py <probe-dir>

Runs `cline auth openai-compatible -k ... -m ... -b ... --config <probe>/cfg --data-dir <probe>/data`
with HOME pointed at <probe>/home, then lists every file it created. Nothing is guessed: the
fixture's providers.json is whatever this wrote.
"""
import os
import subprocess
import sys

base = os.path.abspath(sys.argv[1])
for d in ("cfg", "data", "home", "work"):
    os.makedirs(os.path.join(base, d), exist_ok=True)

env = {k: os.environ[k] for k in ("PATH", "TMPDIR", "LANG", "TERM") if k in os.environ}
env["HOME"] = os.path.join(base, "home")

argv = ["cline", "auth", "openai-compatible",
        "-k", "marion-canned-credential-27cc",
        "-m", "canned-1",
        "-b", "http://127.0.0.1:8127/v1",
        "--config", os.path.join(base, "cfg"),
        "--data-dir", os.path.join(base, "data"),
        "-v"]
p = subprocess.run(argv, env=env, cwd=os.path.join(base, "work"), stdin=subprocess.DEVNULL,
                   capture_output=True, text=True, timeout=60)
print("rc", p.returncode)
print("STDOUT:\n" + p.stdout)
print("STDERR:\n" + p.stderr[-3000:])
for root, _, files in os.walk(base):
    for f in files:
        path = os.path.join(root, f)
        print("FILE", path, os.path.getsize(path))
