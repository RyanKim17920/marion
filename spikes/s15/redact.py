#!/usr/bin/env python3
"""Copy S15's reports into tests/fixtures/s15 with paths redacted.

Home paths become `<HOME>`, the temp directory becomes `<TMP>`, and UUIDs are
masked. **Pids, pgids and sids are left intact** -- they are the evidence, and
the relationships between them are the whole measurement.
"""
import os
import re
import shutil
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
DEST = os.path.abspath(os.path.join(HERE, "..", "..", "tests", "fixtures", "s15"))
HOME = os.path.expanduser("~")
TMP = tempfile.gettempdir().rstrip("/")
UUID = re.compile(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-"
                  r"[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
#: `launchctl print` echoes the temp path in forms `gettempdir()` does not
#: match: with a `/private` prefix, and truncated mid-component by this spike's
#: own `ps` command clipping. Both are the same per-user directory.
TMPDIR = re.compile(r"(/private)?/var/folders/[A-Za-z0-9_+./-]*")

FILES = {
    "s15-report.json": "signal.json",
    "s15-ctty.json": "ctty.json",
    "s15-hangup.json": "hangup.json",
    "s15-launchd.json": "launchd.json",
}


def main():
    os.makedirs(DEST, exist_ok=True)
    for src, dst in FILES.items():
        path = os.path.join(HERE, src)
        if not os.path.exists(path):
            sys.exit("missing report: " + src + " -- run its probe first")
        text = open(path).read()
        text = TMPDIR.sub("<TMP>", text)
        text = text.replace(TMP, "<TMP>").replace(HOME, "<HOME>")
        text = re.sub(r"\b(gui|user)/%d\b" % os.getuid(), r"\1/<UID>", text)
        text = UUID.sub("<UUID>", text)
        with open(os.path.join(DEST, dst), "w") as fh:
            fh.write(text)
        print(dst)


if __name__ == "__main__":
    main()
