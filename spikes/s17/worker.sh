#!/bin/sh
# Two processes doing exactly what spawn::make_worktree / cleanup do, against one repo.
# ITERS iterations each, no sleeps, no coordination. Records every non-zero exit verbatim.
set -u
REPO="$1"; WT="$2"; TAG="$3"; ITERS="$4"; LOG="$5"

i=0
fails=0
while [ "$i" -lt "$ITERS" ]; do
  b="$TAG-$i"
  p="$WT/$b"
  head=$(git -C "$REPO" rev-parse HEAD 2>>"$LOG") || { fails=$((fails+1)); echo "ITER $b rev-parse FAILED" >> "$LOG"; i=$((i+1)); continue; }
  if ! out=$(git -C "$REPO" worktree add -b "$b" "$p" "$head" 2>&1); then
    fails=$((fails+1))
    printf 'ITER %s ADD FAILED\n%s\n' "$b" "$out" >> "$LOG"
    i=$((i+1)); continue
  fi
  if ! out=$(git -C "$REPO" worktree remove --force "$p" 2>&1); then
    fails=$((fails+1))
    printf 'ITER %s REMOVE FAILED\n%s\n' "$b" "$out" >> "$LOG"
  fi
  if ! out=$(git -C "$REPO" branch -D "$b" 2>&1); then
    fails=$((fails+1))
    printf 'ITER %s BRANCH-D FAILED\n%s\n' "$b" "$out" >> "$LOG"
  fi
  i=$((i+1))
done
echo "$TAG done: $fails failures over $ITERS iterations" >> "$LOG"
