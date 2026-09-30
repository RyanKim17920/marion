#!/usr/bin/env python3
"""Turn a passed admission run (scripts/sandbox-admit.sh) into a row's `admitted` note.

    scripts/sandbox-admit-apply.py target/sandbox-admit/claude.json
    scripts/sandbox-admit-apply.py target/sandbox-admit/claude.json --not-the-node ~/Library/Safari

A run passes when marion exited 0, the task's hello.txt exists, and every file written under $HOME
since the run began lies inside the row's list. Under the profile the node cannot write anywhere
else, so a path outside is another process's: the harness's own daemon outside the launched tree
(which fails the row), or an unrelated app. Name the latter with --not-the-node <prefix>, once you
have looked; each one is recorded in the note, so the admission says what was set aside.

On a pass, the row's `admitted: None` inside its `live: crate::os_sandbox::Live { … }` becomes
`admitted: Some("<version>, <date>: <task>")`. Nothing else in the file changes; run `cargo fmt`
and the row's tests, then commit it with the result file's numbers in the message.
"""

import argparse
import json
import os
import re
import sys

ROWS = {"claude": "claude_code.rs", "codex": "codex.rs", "opencode": "opencode.rs", "pi": "pi.rs"}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("result")
    ap.add_argument("--not-the-node", action="append", default=[], metavar="PREFIX")
    a = ap.parse_args()
    r = json.load(open(a.result))
    h = r["harness"]
    if "blocked" in r:
        print(f"{h}: BLOCKED ({r['blocked']}); nothing to admit")
        return 1
    ignored = [os.path.realpath(os.path.expanduser(p)) for p in a.not_the_node]
    outside = [
        p for p in r["outside"]
        if not any(os.path.realpath(p).startswith(i) for i in ignored)
    ]
    failures = []
    if r["exit"] != 0:
        failures.append(f"marion exited {r['exit']}; stderr ends:\n{r['stderr_tail']}")
    if not r["hello"]:
        failures.append("the task's hello.txt was not written")
    if outside:
        failures.append("written outside the row's list:\n  " + "\n  ".join(outside))
    if failures:
        print(f"{h}: not admitted\n" + "\n".join(failures))
        return 1
    note = f"{r['version']}, {r['date']}: {r['task']}"
    if ignored:
        note += "; set aside as not the node's: " + ", ".join(a.not_the_node)
    root = os.path.dirname(os.path.dirname(os.path.realpath(__file__)))
    path = os.path.join(root, "crates/marion-harness/src", ROWS[h])
    src = open(path).read()
    block = re.search(r"live: crate::os_sandbox::Live \{.*?admitted: None,", src, re.S)
    if not block:
        print(f"{h}: {path} has no unadmitted `live` list to admit")
        return 1
    edited = block.group(0).replace("admitted: None,", f"admitted: Some({json.dumps(note)}),")
    open(path, "w").write(src[: block.start()] + edited + src[block.end():])
    print(f"{h}: admitted in {path}: {note}\nnext: cargo fmt -p marion-harness, then commit")
    return 0


if __name__ == "__main__":
    sys.exit(main())
