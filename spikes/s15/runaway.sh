#!/bin/sh
# S7 case B -- "still running": started by codex's `exec_command` with a short
# yield_time_ms, so `exec` yields back to the model while this is STILL ALIVE.
# That is the shape of a genuine runaway tool call at marion's timeout expiry:
# a live foreground child plus a backgrounded grandchild.
OUT="$1"
: > "$OUT"
echo "runshell $$" >> "$OUT"

/bin/sleep 900 &
echo "runchild $!" >> "$OUT"

echo "done" >> "$OUT"
exec /bin/sleep 900
