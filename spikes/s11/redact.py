#!/usr/bin/env python3
"""Turn an S11 run directory into a committable fixture.

Fixtures are recordings: nothing here is hand-written. This script only substitutes.
House style follows tests/fixtures/s6/README.md and tests/fixtures/s9/README.md:

  * home paths -> <HOME>, the per-run scratch dir -> <SCRATCH> (with and without macOS's
    /private prefix)
  * UUIDs -> <UUID-1>, <UUID-2>, ... numbered by first appearance (S9's deviation from
    S6's single <UUID>, kept because frame-to-frame correlation is evidence here too)
  * *reduced, not merely scrubbed* (S9's word): the `initialize` reply's catalogues and the
    `system/init` frame's machine-specific lists are replaced with
    "<REDACTED-machine-specific>". The keys stay so the shape still reads. None of it is
    evidence about framing and all of it is specific to the operator's machine.
  * hook bodies -> <REDACTED_HOOK_OUTPUT>

`raw.bin` is deliberately NOT copied: it is the unredacted byte stream.

    python3 redact.py RUNDIR OUTDIR SCRATCH_ROOT
"""
import json
import os
import re
import sys

UUID_RX = re.compile(
    r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b")

INIT_REPLY_REDUCE = ["commands", "agents", "models", "available_output_styles",
                     "account", "pid"]
SYSTEM_INIT_REDUCE = ["slash_commands", "skills", "agents", "plugins", "mcp_servers",
                      "memory_paths", "tools", "cwd", "apiKeySource", "output_style",
                      "permissionMode", "model"]
HOOK_REDUCE = ["output", "stdout", "stderr", "hook_name", "hook_event"]


class Redactor:
    def __init__(self, scratch):
        self.uuids = {}
        home = os.path.expanduser("~")
        self.paths = []
        for p in (scratch, "/private" + scratch if not scratch.startswith("/private")
                  else scratch[len("/private"):]):
            if p:
                self.paths.append((p, "<SCRATCH>"))
        self.paths.append((home, "<HOME>"))
        self.paths.sort(key=lambda kv: -len(kv[0]))

    def uuid_sub(self, m):
        v = m.group(0)
        if v not in self.uuids:
            self.uuids[v] = "<UUID-%d>" % (len(self.uuids) + 1)
        return self.uuids[v]

    def text(self, s):
        for src, dst in self.paths:
            s = s.replace(src, dst)
        return UUID_RX.sub(self.uuid_sub, s)

    def obj(self, o):
        """Reduce first (so the big machine-specific bodies never reach the text pass)."""
        if isinstance(o, dict):
            t, st = o.get("type"), o.get("subtype")
            if t == "system" and st == "init":
                o = dict(o)
                for k in SYSTEM_INIT_REDUCE:
                    if k in o:
                        o[k] = "<REDACTED-machine-specific>"
            if t == "system" and st in ("hook_started", "hook_response"):
                o = dict(o)
                for k in HOOK_REDUCE:
                    if k in o:
                        o[k] = "<REDACTED_HOOK_OUTPUT>"
            if t == "control_response":
                resp = o.get("response", {})
                inner = resp.get("response")
                if isinstance(inner, dict) and any(k in inner for k in INIT_REPLY_REDUCE):
                    o = json.loads(json.dumps(o))
                    ir = o["response"]["response"]
                    for k in list(ir):
                        if k in INIT_REPLY_REDUCE:
                            ir[k] = "<REDACTED-machine-specific>"
            return {k: self.obj(v) for k, v in o.items()}
        if isinstance(o, list):
            return [self.obj(v) for v in o]
        if isinstance(o, str):
            return self.text(o)
        return o


def main():
    rundir, outdir, scratch = sys.argv[1], sys.argv[2], sys.argv[3]
    os.makedirs(outdir, exist_ok=True)
    r = Redactor(scratch)

    # stdout.jsonl — one line per frame, in arrival order, with arrival time and the index
    # of the read() chunk that completed it. The chunk index is the framing evidence.
    src = os.path.join(rundir, "stdout.jsonl")
    if os.path.exists(src):
        with open(os.path.join(outdir, "stdout.jsonl"), "w") as fh:
            for line in open(src):
                if line.strip():
                    fh.write(json.dumps(r.obj(json.loads(line))) + "\n")

    for name in ("chunks.jsonl",):
        src = os.path.join(rundir, name)
        if os.path.exists(src):
            with open(os.path.join(outdir, name), "w") as fh:
                for line in open(src):
                    fh.write(line)

    src = os.path.join(rundir, "summary.json")
    if os.path.exists(src):
        d = r.obj(json.load(open(src)))
        with open(os.path.join(outdir, "summary.json"), "w") as fh:
            json.dump(d, fh, indent=2)
    print("redacted %s -> %s (%d uuids)" % (rundir, outdir, len(r.uuids)))


if __name__ == "__main__":
    main()
