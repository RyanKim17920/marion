#!/usr/bin/env python3
"""Summarise one S27 run directory: stdout frame types, provider requests, MCP methods.

    inspect.py <label> [--full]
"""
import json
import os
import sys

OUTROOT = os.getenv("S27_OUTROOT") or (
    "/private/tmp/claude-501/-Users-ryankim-Desktop-CODING-marion/"
    "d0851ffa-1d87-449b-ad18-eebc26b60703/scratchpad/cline")
run = os.path.join(OUTROOT, sys.argv[1])
full = "--full" in sys.argv

meta = json.load(open(os.path.join(run, "meta.json")))
print("rc", meta["rc"], "timed_out", meta["timed_out"], "elapsed", meta["elapsed_s"],
      "home_changed", meta["home_cline_changed"])
print("procs_after:", meta["cline_procs_after"])
print("data tree:", [t["path"] for t in meta["data_tree_after"]])
print("cfg tree:", [t["path"] for t in meta["cfg_tree_after"]])
print("home tree:", [t["path"] for t in meta["home_tree_after"]])
print("work tree:", [t["path"] for t in meta["work_tree_after"]])

print("\n=== stderr")
print(open(os.path.join(run, "stderr.txt")).read()[:3000])

print("\n=== stdout frames")
for line in open(os.path.join(run, "stdout.jsonl")):
    line = line.strip()
    if not line:
        continue
    try:
        f = json.loads(line)
    except json.JSONDecodeError:
        print("NONJSON", line[:300])
        continue
    if full:
        print(json.dumps(f)[:1500])
    else:
        print(json.dumps(f)[:400])

print("\n=== provider requests")
p = os.path.join(run, "provider-requests.jsonl")
if os.path.exists(p):
    for line in open(p):
        r = json.loads(line)
        b = r.get("body", {})
        print(r.get("seq"), r["method"], r["path"], r["kind"], "tools=", r.get("tool_names"))
        print("   body keys", sorted(b.keys()))
        for m in b.get("messages", []):
            c = m.get("content")
            print("   msg", m.get("role"), len(json.dumps(m)),
                  (c[:200] if isinstance(c, str) else json.dumps(c)[:200]))

print("\n=== mcp")
p = os.path.join(run, "mcp.jsonl")
if os.path.exists(p):
    for line in open(p):
        r = json.loads(line)
        fr = r["frame"]
        print(r["dir"], fr.get("method"), json.dumps(fr)[:600])
