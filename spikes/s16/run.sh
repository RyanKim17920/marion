#!/bin/sh
# S16 -- what a harness exit does to its MCP stdio server, and to that server's
# grandchild.
#
#   sh spikes/s16/run.sh OUTDIR [PORT] [REPEATS]
#
# The control runs FIRST and on purpose. It is the run that proves the probe's
# log can record an EOF, a survival and a reparent; if it comes back silent then
# every subsequent silence in the harness runs means nothing, and S7's lesson is
# that a leak check which cannot see a survivor is worse than none.
#
# REPEATS (default 3) applies to the harness case. One run cannot distinguish a
# teardown policy from a race, and the SIGINT->SIGTERM gap is the kind of number
# that deserves a spread rather than a point.
#
# Costs nothing: ANTHROPIC_BASE_URL points at 127.0.0.1 and no model is called.
set -e
OUT="${1:?usage: run.sh OUTDIR [PORT] [REPEATS]}"
PORT="${2:-8116}"
REPEATS="${3:-3}"
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$OUT"

echo "=== control (no harness at all) ==="
S16_REPORT="$HERE/s16-control.json" \
  python3 "$HERE/run_probe.py" control "$OUT/control"

N=1
while [ "$N" -le "$REPEATS" ]; do
  echo "=== harness rep$N ==="
  if [ "$N" -eq 1 ]; then R="$HERE/s16-harness.json"; else R="$HERE/s16-harness.rep$N.json"; fi
  # A fresh provider per run: the provider's turn counter is per-process, as in
  # S11. A fresh port too, so a lingering socket from the previous rep cannot
  # answer this one.
  S16_REPORT="$R" S16_PORT="$((PORT + N - 1))" \
    python3 "$HERE/run_probe.py" harness "$OUT/harness-rep$N" "$((PORT + N - 1))"
  N=$((N + 1))
done

echo "=== harness, claude in a process group of its OWN (marion's arrangement) ==="
# Removes the confound in the runs above: there, claude shared the runner's
# process group, so killpg was not a thing it could have used. Here it is the
# group leader and its MCP server and grandchild are the only other members.
S16_REPORT="$HERE/s16-harness-own-pgroup.json" S16_PORT="$((PORT + 20))" \
  python3 "$HERE/run_probe.py" harness-own-pgroup "$OUT/harness-own-pgroup" \
  "$((PORT + 20))"

echo "=== leak check ==="
pgrep -fl mcp_probe.py || echo "no mcp_probe.py"
pgrep -fl grandchild.py || echo "no grandchild.py"
pgrep -fl s16/canned_provider.py || echo "no s16 canned_provider.py"
