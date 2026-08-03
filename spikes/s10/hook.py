#!/usr/bin/env python3
"""S10 Stop/SubagentStop hook. Records every fire verbatim; optionally blocks.

Appends one line per fire to $S10_HOOKLOG:
    {"fire": N, "raw": "<the exact bytes read from stdin>"}

`raw` is the *verbatim* stdin text, so the fixture is a recording rather than a
re-serialisation of a parsed dict.

If S10_BLOCK=1 it emits `{"decision":"block","reason":…}` on the first
`SubagentStop` fire only -- never when `stop_hook_active` is true, which is the
§7.6 loop guard marion itself obeys.
"""
import json
import os
import sys

HOOKLOG = os.environ["S10_HOOKLOG"]
BLOCK = os.getenv("S10_BLOCK") == "1"
REASON = os.getenv("S10_REASON", "marion s10: report before you stop.")

raw = sys.stdin.read()

n = 0
if os.path.exists(HOOKLOG):
    with open(HOOKLOG) as fh:
        n = sum(1 for line in fh if line.strip())
with open(HOOKLOG, "a") as fh:
    fh.write(json.dumps({"fire": n + 1, "raw": raw}) + "\n")

try:
    payload = json.loads(raw)
except Exception:
    payload = {}

event = payload.get("hook_event_name")
active = payload.get("stop_hook_active")

if BLOCK and event == "SubagentStop" and not active:
    sys.stdout.write(json.dumps({"decision": "block", "reason": REASON}))
sys.exit(0)
