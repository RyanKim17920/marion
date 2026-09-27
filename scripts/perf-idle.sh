#!/bin/sh
# Idle-cost probe for one running process (macOS): how often it wakes, how much CPU it burns and
# how much memory it holds while nothing is happening. marion's long-lived processes (the
# supervisor daemon, a native facade relay, `marion tree`, a pane attach) should sit near zero on
# every column when idle; this is the number a perf commit quotes before and after.
#
#   scripts/perf-idle.sh <pid> [seconds=60]
#
# Columns: rss_kib and threads at the end of the window; cpu_ms is CPU time consumed during the
# window (user+sys, from top's TIME); csw/s is context switches per second — each timer-driven
# wakeup costs at least one, so it is the idle-wakeup rate; bsd/s and mach/s are syscalls per
# second. No root needed (powermetrics would be, and dtrace needs SIP off).
set -eu
pid=${1:?usage: perf-idle.sh <pid> [seconds]}
secs=${2:-60}
[ "$(uname)" = Darwin ] || { echo "perf-idle.sh: macOS only (uses top -l)" >&2; exit 2; }
kill -0 "$pid" 2>/dev/null || { echo "perf-idle.sh: no process $pid" >&2; exit 1; }

# top prints cumulative counters for its first sample and the same counters again after the
# interval; the difference of the two samples is the window. TIME is [hh:]mm:ss.cc.
top -l 2 -s "$secs" -stats pid,csw,sysbsd,sysmach,time -pid "$pid" \
  | awk -v pid="$pid" -v secs="$secs" '
      function ms(t,   n, a) { gsub(/\+/, "", t); n = split(t, a, ":");
        return n == 3 ? ((a[1] * 60 + a[2]) * 60 + a[3]) * 1000 : (a[1] * 60 + a[2]) * 1000 }
      function num(v) { gsub(/[+-]/, "", v); return v + 0 }
      $1 == pid { i++; csw[i] = num($2); bsd[i] = num($3); mach[i] = num($4); t[i] = ms($5) }
      END {
        if (i < 2) { print "perf-idle.sh: process exited during the window" > "/dev/stderr"; exit 1 }
        printf "cpu_ms=%d csw/s=%.1f bsd/s=%.1f mach/s=%.1f\n",
          t[2] - t[1], (csw[2] - csw[1]) / secs, (bsd[2] - bsd[1]) / secs, (mach[2] - mach[1]) / secs
      }'
printf 'rss_kib=%s threads=%s\n' "$(ps -o rss= -p "$pid" | tr -d ' ')" \
  "$(($(ps -M -p "$pid" | wc -l) - 1))"
