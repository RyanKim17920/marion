#!/bin/sh
# S17 — does `git worktree add` / `remove` from N concurrent PROCESSES against one repository fail?
#
# `spawn::repo_write_guard` serializes marion's own worktree calls *within one process* and its doc
# comment cited a git behaviour it had no witness for. This probe is that witness, or its absence.
# Nothing here uses marion: the question is about git, so the probe is git.
#
#   ./run.sh <workers> <iterations-each> [repetitions]
#
# Each worker runs `rev-parse HEAD`, `worktree add -b <tag>-<i>`, `worktree remove --force`,
# `branch -D` in a tight loop with no sleeps and no coordination — exactly `spawn::make_worktree`
# and `spawn::cleanup`'s calls, in their order. Every non-zero exit is logged verbatim, because the
# shape of the failure is the finding and a count would hide it.
set -eu

WORKERS="${1:-6}"
ITERS="${2:-200}"
REPS="${3:-3}"

here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

repo="$work/repo"
mkdir -p "$repo/src"
printf 'keep\n' > "$repo/src/keep.txt"
git -C "$repo" init -q -b main .
git -C "$repo" add -A
git -C "$repo" -c user.email=marion@example.invalid -c user.name=marion commit -qm fixture

git --version
echo "workers=$WORKERS iterations=$ITERS repetitions=$REPS"

rep=1
while [ "$rep" -le "$REPS" ]; do
  rm -rf "$work/wt"; mkdir -p "$work/wt"
  git -C "$repo" worktree prune >/dev/null 2>&1 || true
  for b in $(git -C "$repo" branch --format='%(refname:short)' | grep -v '^main$' || true); do
    git -C "$repo" branch -D "$b" >/dev/null 2>&1 || true
  done

  log="$work/log.$rep"
  : > "$log"
  n=1
  while [ "$n" -le "$WORKERS" ]; do
    "$here/worker.sh" "$repo" "$work/wt" "r${rep}w${n}" "$ITERS" "$log" &
    n=$((n + 1))
  done
  wait

  echo "=== repetition $rep ==="
  cat "$log"
  rep=$((rep + 1))
done
