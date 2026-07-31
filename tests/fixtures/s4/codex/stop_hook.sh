#!/bin/bash
D=<SCRATCH>
IN=$(cat)
MODE=$(cat "$D/CODEX_MODE" 2>/dev/null || echo none)
mkdir -p "$D/logs"
echo "$IN" >> "$D/logs/codex-hook-input-$MODE.jsonl"
ACTIVE=$(echo "$IN" | /usr/bin/python3 -c 'import sys,json;print(json.load(sys.stdin).get("stop_hook_active"))' 2>/dev/null)
EV=$(echo "$IN" | /usr/bin/python3 -c 'import sys,json;print(json.load(sys.stdin).get("hook_event_name",""))' 2>/dev/null)
echo "{\"ts\":\"$(date +%s)\",\"mode\":\"$MODE\",\"event\":\"$EV\",\"stop_hook_active\":\"$ACTIVE\"}" >> "$D/logs/codex-hook-fired-$MODE.jsonl"
if [ "$ACTIVE" = "True" ] || [ "$ACTIVE" = "true" ]; then echo '{}'; exit 0; fi
case "$MODE" in
  block) echo '{"decision":"block","reason":"MARION-S4: are you reporting a result, or are you waiting on something? Answer with exactly the word GOLF."}' ;;
  exit2) echo "MARION-S4: are you reporting a result, or are you waiting on something? Answer with exactly the word HOTEL." >&2; exit 2 ;;
  addctx) echo '{"hookSpecificOutput":{"hookEventName":"Stop","additionalContext":"MARION-S4: answer with exactly the word INDIA."}}' ;;
  *) echo '{}' ;;
esac
exit 0
