#!/bin/bash
# S10 -- drive a real `claude` through a Task subagent against a canned provider,
# with Stop and SubagentStop hooks registered, and record everything.
#
#   ./run_probe.sh <outdir> [block]
#
# `block` makes the hook answer {"decision":"block","reason":…} on the first
# SubagentStop fire. No model is called; no paid tokens are spent.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="${1:?usage: run_probe.sh <outdir> [block]}"
MODE="${2:-none}"
PORT="${S10_PORT:-8110}"

mkdir -p "$OUT"
export S10_REQLOG="$OUT/requests.jsonl"
export S10_HOOKLOG="$OUT/hook-input.jsonl"
export S10_PORT="$PORT"
: > "$S10_REQLOG"
: > "$S10_HOOKLOG"
[ "$MODE" = block ] && export S10_BLOCK=1 || unset S10_BLOCK

# Isolated everything: a throwaway config dir and a throwaway cwd, so no hook is
# ever written into the operator's real ~/.claude and no project settings leak in.
CFG="$OUT/claude-config"
CWD="$OUT/cwd"
mkdir -p "$CFG" "$CWD"

# The double-nested hook shape the design records. A typo at either level is
# silently ignored, so this literal is the thing to keep exact.
CMD="S10_HOOKLOG='$S10_HOOKLOG' ${S10_BLOCK:+S10_BLOCK=1 }python3 '$HERE/hook.py'"
if [ "$MODE" = badshape ]; then
  # NEGATIVE CONTROL: single-nested (the inner "hooks" array omitted) and one
  # misspelled event key. Both are expected to be silently ignored -- no warning,
  # no non-zero exit, simply no fire. This is what a wrong shape looks like, so a
  # real "never fires" can be told apart from a typo.
  cat > "$OUT/settings.json" <<JSON
{
  "hooks": {
    "SubagentStop": [{"type": "command", "command": "$CMD"}],
    "SubAgentStop": [{"hooks": [{"type": "command", "command": "$CMD"}]}],
    "subagent_stop": [{"hooks": [{"type": "command", "command": "$CMD"}]}]
  }
}
JSON
else
  cat > "$OUT/settings.json" <<JSON
{
  "hooks": {
    "SubagentStop": [
      {"hooks": [{"type": "command", "command": "$CMD"}]}
    ],
    "Stop": [
      {"hooks": [{"type": "command", "command": "$CMD"}]}
    ]
  }
}
JSON
fi

python3 "$HERE/canned_provider.py" > "$OUT/provider.stderr.txt" 2>&1 &
PROV=$!
trap 'kill $PROV 2>/dev/null' EXIT
for _ in $(seq 1 50); do
  nc -z 127.0.0.1 "$PORT" 2>/dev/null && break
  sleep 0.1
done

cd "$CWD" || exit 1
env -i \
  PATH="$PATH" HOME="$HOME" TERM=dumb \
  CLAUDE_CONFIG_DIR="$CFG" \
  ANTHROPIC_BASE_URL="http://127.0.0.1:$PORT" \
  ANTHROPIC_API_KEY="s10-canned-no-real-key" \
  ANTHROPIC_MODEL="canned-model" \
  DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1 DISABLE_AUTOUPDATER=1 \
  DISABLE_NON_ESSENTIAL_MODEL_CALLS=1 \
  claude -p "spawn one subagent" \
    --output-format stream-json --verbose \
    --include-hook-events --forward-subagent-text \
    --settings "$OUT/settings.json" \
    --setting-sources "" \
    --permission-mode bypassPermissions \
    --tools Task \
    --no-session-persistence \
  > "$OUT/stream.jsonl" 2> "$OUT/claude.stderr.txt"
echo "claude exit: $?" >> "$OUT/claude.stderr.txt"

kill $PROV 2>/dev/null
wait $PROV 2>/dev/null
echo "wrote $OUT"
