#!/bin/bash
D="$(dirname "$0")"; LOGDIR="$D/logs"; mkdir -p "$LOGDIR"
IN=$(cat); MODE=$(cat "$D/MODE" 2>/dev/null || echo none)
EV=$(echo "$IN" | /usr/bin/python3 -c 'import sys,json;print(json.load(sys.stdin).get("hook_event_name",""))' 2>/dev/null)
ACTIVE=$(echo "$IN" | /usr/bin/python3 -c 'import sys,json;print(json.load(sys.stdin).get("stop_hook_active"))' 2>/dev/null)
echo "$IN" >> "$LOGDIR/hook-input-$MODE.jsonl"
echo "{\"ts\":\"$(date +%s)\",\"mode\":\"$MODE\",\"event\":\"$EV\",\"stop_hook_active\":\"$ACTIVE\"}" >> "$LOGDIR/hook-fired-$MODE.jsonl"
[ "$ACTIVE" = "True" ] || [ "$ACTIVE" = "true" ] && exit 0
WORD=BRAVO; [ "$EV" = "SubagentStop" ] && WORD=DELTA
case "$MODE" in
  block) echo "{\"decision\":\"block\",\"reason\":\"MARION-S4 ($EV): are you reporting a result, or are you waiting on something? Answer with exactly the word $WORD.\"}" ;;
  additionalContext) echo "{\"hookSpecificOutput\":{\"hookEventName\":\"$EV\",\"additionalContext\":\"MARION-S4 ($EV): are you reporting a result, or are you waiting on something? Answer with exactly the word $WORD.\"}}" ;;
  *) exit 0 ;;
esac
exit 0
