#!/bin/sh
# S11 — S1's interrupt protocol over pipes, a pty stdout, a pty stdin, and a full pty.
#
# One fresh canned provider per run (the provider's turn counter is per-process, and turn 1
# is the long interruptible stream), one fresh workspace per transport.
#
#   sh spikes/s11/run.sh OUTDIR [PORT] [REPEATS]
#
# REPEATS (default 3) applies to the two transports that complete a session; the two that
# stdin-refuse are run once. Repeats exist because design §11 item 11 flags that S1's
# headline latencies rest on a single run — one new run cannot answer that, but three at
# least show the spread.
#
# Costs nothing: ANTHROPIC_BASE_URL points at 127.0.0.1 and no model is ever called.
set -e
OUT="${1:?usage: run.sh OUTDIR [PORT] [REPEATS]}"
PORT="${2:-8111}"
REPEATS="${3:-3}"
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$OUT"

one_run() {
  T="$1"; DIR="$2"
  S11_PORT="$PORT" S11_REQLOG="$DIR.requests.jsonl" \
    python3 "$HERE/canned_provider.py" >"$DIR.provider.log" 2>&1 &
  PROV=$!
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    curl -sf "http://127.0.0.1:$PORT/" >/dev/null && break
    sleep 0.3
  done
  mkdir -p "$OUT/wt-$T"
  python3 "$HERE/pty_interrupt.py" --transport "$T" --out "$DIR" \
    ${RAW:+--raw-output} \
    --base-url "http://127.0.0.1:$PORT" --cwd "$OUT/wt-$T" || true
  RAW=""
  kill "$PROV" 2>/dev/null || true
  wait "$PROV" 2>/dev/null || true
  sleep 0.4
}

for T in pipes pty-out; do
  N=1
  while [ "$N" -le "$REPEATS" ]; do
    if [ "$N" -eq 1 ]; then DIR="$OUT/run-$T"; else DIR="$OUT/run-$T.rep$N"; fi
    echo "=== $T rep$N ==="
    one_run "$T" "$DIR"
    N=$((N + 1))
  done
done

echo "=== pty-out-raw (OPOST cleared: attributes the \\r to ONLCR) ==="
RAW=1 one_run pty-out "$OUT/run-pty-out-raw"

for T in pty-in pty-all; do
  echo "=== $T ==="
  one_run "$T" "$OUT/run-$T"
done

echo "=== compare ==="
python3 "$HERE/compare.py" "$OUT" >/dev/null
echo "wrote $OUT/compare.json"
