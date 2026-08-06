#!/usr/bin/env python3
"""Copy S16's reports into tests/fixtures/s16 with paths redacted.

S15's redactor, kept deliberately close to it: home paths become `<HOME>`, the
per-run scratch directory becomes `<SCRATCH>` (with and without macOS's
`/private` prefix), the temp directory becomes `<TMP>`, and UUIDs are masked.

**Pids, ppids, pgids and sids are left intact** -- they are the evidence, and
the relationships between them (probe's ppid becoming 1, grandchild sharing the
harness's process group) are the whole measurement. So are the monotonic
timestamps: the ordering of SIGINT, SIGTERM, the last heartbeat and the
harness's own exit is the finding.

    python3 spikes/s16/redact.py [SCRATCH_ROOT]
"""
import os
import re
import shutil
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
DEST = os.path.abspath(os.path.join(HERE, "..", "..", "tests", "fixtures", "s16"))
HOME = os.path.expanduser("~")
TMP = tempfile.gettempdir().rstrip("/")
UUID = re.compile(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-"
                  r"[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
TMPDIR = re.compile(r"(/private)?/var/folders/[A-Za-z0-9_+./-]*")


def reports():
    """Every s16-*.json beside this script, including the repeats."""
    return sorted(f for f in os.listdir(HERE)
                  if f.startswith("s16-") and f.endswith(".json"))


def main():
    scratch = os.path.abspath(sys.argv[1]) if len(sys.argv) > 1 else None
    subs = []
    if scratch:
        subs.append((scratch, "<SCRATCH>"))
        alt = ("/private" + scratch if not scratch.startswith("/private")
               else scratch[len("/private"):])
        subs.append((alt, "<SCRATCH>"))
    subs.append((TMP, "<TMP>"))
    subs.append((HOME, "<HOME>"))
    subs.sort(key=lambda kv: -len(kv[0]))

    found = reports()
    if not found:
        sys.exit("no s16-*.json beside redact.py -- run spikes/s16/run.sh first")
    os.makedirs(DEST, exist_ok=True)
    for src in found:
        text = open(os.path.join(HERE, src)).read()
        text = TMPDIR.sub("<TMP>", text)
        for a, b in subs:
            text = text.replace(a, b)
        text = UUID.sub("<UUID>", text)
        # `s16-harness.json` -> `harness.json`, `s16-harness.rep2.json` ->
        # `harness.rep2.json`; the fixture directory already says s16.
        dst = src[len("s16-"):]
        with open(os.path.join(DEST, dst), "w") as fh:
            fh.write(text)
        print(dst)


if __name__ == "__main__":
    main()
