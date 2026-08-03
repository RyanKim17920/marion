#!/bin/sh
# S7 case A -- "orphan": started by codex's `exec_command`, backgrounds two
# sleepers and then EXITS. The tool call therefore completes while the sleepers
# are still running: they are orphaned runaways.
OUT="$1"
: > "$OUT"
echo "toolshell $$" >> "$OUT"

/bin/sleep 900 &
echo "child $!" >> "$OUT"

/bin/sh -c "/bin/sleep 900 & echo grandchild \$! >> $OUT; exit 0" &
echo "midshell $!" >> "$OUT"

sleep 1
echo "done" >> "$OUT"
