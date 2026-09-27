#!/bin/sh
# Stop-hook probe. Usage: gate-hook.sh <tag> <logdir>
# Appends the hook's stdin JSON to <logdir>/<tag>.jsonl. On the first call for a session it
# prints a block decision whose reason carries a nonce the model must echo; on any later call
# (or when Claude Code says stop_hook_active) it allows the stop.
tag="$1"; dir="$2"
mkdir -p "$dir"
input="$(cat)"
printf '%s\n' "$input" >> "$dir/$tag.jsonl"
marker="$dir/$tag.blocked"
case "$input" in
  *'"stop_hook_active":true'*) exit 0 ;;
esac
if [ -e "$marker" ]; then exit 0; fi
: > "$marker"
printf '{"decision":"block","reason":"GATE-%s: before stopping, reply with exactly the word PINEAPPLE-%s and nothing else."}\n' "$tag" "$tag"
