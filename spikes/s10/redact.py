#!/usr/bin/env python3
"""Turn an S10 run directory into the redacted fixture files.

    ./redact.py <rundir> <label> <fixture-dir>

Substitutions (house style, S9 numbering):
  * $HOME                     -> <HOME>
  * the run directory         -> <SCRATCH>   (with and without macOS /private)
  * UUIDs                     -> <UUID-N>, numbered by first appearance
  * Claude Code agent ids      -> <AGENT-ID-N>, likewise
Nothing else is touched; every remaining byte is as the CLI wrote it.
"""
import json
import os
import re
import sys

rundir, label, fixdir = sys.argv[1], sys.argv[2], sys.argv[3]
rundir = os.path.abspath(rundir)
home = os.path.expanduser("~")

UUID = re.compile(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
# Claude Code task/agent ids: 17 lowercase hex chars, no dashes.
AGENT = re.compile(r"\b[0-9a-f]{17}\b")

_uuids, _agents = {}, {}


def redact(s):
    for pre in ("/private" + rundir, rundir):
        s = s.replace(pre, "<SCRATCH>")
        s = s.replace(pre.replace("/", "-"), "<SCRATCH-SLUG>")
    for pre in ("/private" + home, home):
        s = s.replace(pre, "<HOME>")
        s = s.replace(pre.replace("/", "-"), "<HOME-SLUG>")

    def u(m):
        return _uuids.setdefault(m.group(0), "<UUID-%d>" % (len(_uuids) + 1))

    def a(m):
        return _agents.setdefault(m.group(0), "<AGENT-ID-%d>" % (len(_agents) + 1))

    return AGENT.sub(a, UUID.sub(u, s))


os.makedirs(fixdir, exist_ok=True)

# hook input: ship the verbatim payload lines, not the {"fire":…,"raw":…} wrapper
src = os.path.join(rundir, "hook-input.jsonl")
dst = os.path.join(fixdir, "hook-input-%s.jsonl" % label)
with open(src) as fh, open(dst, "w") as out:
    n = 0
    for line in fh:
        if not line.strip():
            continue
        out.write(redact(json.loads(line)["raw"].strip()) + "\n")
        n += 1
print("%s: %d hook fire(s)" % (dst, n))

for name, out_name in (("stream.jsonl", "stream-%s.jsonl" % label),
                       ("settings.json", "settings-%s.json" % label)):
    s = os.path.join(rundir, name)
    if not os.path.exists(s):
        continue
    with open(s) as fh:
        body = fh.read()
    with open(os.path.join(fixdir, out_name), "w") as out:
        out.write(redact(body))
    print(os.path.join(fixdir, out_name))
