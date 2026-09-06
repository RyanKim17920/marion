#!/usr/bin/env python3
"""S27 step 0b: discover the on-disk MCP settings shape by letting `cline mcp install` write it.

    mcp_probe.py <probe-dir>

Same isolated tree as auth_probe.py. The stdio command is a placeholder; only the file shape
matters here.
"""
import os
import subprocess
import sys

base = os.path.abspath(sys.argv[1])
for d in ("cfg", "data", "home", "work"):
    os.makedirs(os.path.join(base, d), exist_ok=True)

env = {k: os.environ[k] for k in ("PATH", "TMPDIR", "LANG", "TERM") if k in os.environ}
env["HOME"] = os.path.join(base, "home")
env["CLINE_DIR"] = os.path.join(base, "cfg")
env["CLINE_DATA_DIR"] = os.path.join(base, "data")

argv = ["cline", "mcp", "install", "marion", "--yes", "--json", "--transport", "stdio",
        "--", "python3", "/tmp/placeholder_mcp.py", "/tmp/placeholder.log"]
p = subprocess.run(argv, env=env, cwd=os.path.join(base, "work"), stdin=subprocess.DEVNULL,
                   capture_output=True, text=True, timeout=60)
print("rc", p.returncode)
print("STDOUT:\n" + p.stdout)
print("STDERR:\n" + p.stderr[-3000:])
for root, _, files in os.walk(base):
    for f in files:
        path = os.path.join(root, f)
        print("FILE", path, os.path.getsize(path))
        if f.endswith(".json"):
            print(open(path).read())
