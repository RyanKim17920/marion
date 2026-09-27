#!/bin/sh
# Re-run the S35 Stop-hook probes against the installed claude, on the operator's existing login.
# Usage: S35_SCRATCH=<empty dir> DRIVER=<demo-driver> run.sh
# Needs an interactive claude.ai login (channels are refused under API-key auth). Each case is one
# haiku turn plus one gate continuation (~$0.02-0.09 notional). Never edits the operator's settings.
set -u
FX=$(cd "$(dirname "$0")" && pwd)
SP=${S35_SCRATCH:?set S35_SCRATCH}
DRIVER=${DRIVER:?set DRIVER to the demo-driver binary}
CLAUDE=${CLAUDE:-$(whence -p claude 2>/dev/null || command -v claude)}
mkdir -p "$SP/proj"
[ -d "$SP/proj/.git" ] || (cd "$SP/proj" && git init -q . && echo hi > README && git add -A &&
  git -c user.email=x@x -c user.name=x commit -qm init)
printf '{"mcpServers":{"s35probe":{"command":"%s/channel-server.py","args":[]}}}\n' "$FX" > "$SP/mcp.json"

# settings <tag> <case> [extra-json]: an overlay whose only hook is the Stop probe.
settings() {
  python3 -c 'import json,sys
t,c,fx,sp=sys.argv[1:5]; extra=json.loads(sys.argv[5] or "{}")
s={"hooks":{"Stop":[{"hooks":[{"type":"command","command":f"{fx}/gate-hook.sh {t} {sp}/{c}/hooklog"}]}]}}
s.update(extra); print(json.dumps(s))' "$1" "$2" "$FX" "$SP" "${3:-}"
}

# case <name> <turn-steps> [claude args...]: dialogs, one turn, snapshot, teardown.
case_run() {
  C="$SP/$1"; T="$FX/$2"; shift 2
  mkdir -p "$C"; rm -rf "$C"/s.* "$C/debug.log" "$C/hooklog"; : > "$C/steps"
  tail -f "$C/steps" | env -u CLAUDECODE -u CLAUDE_CODE_ENTRYPOINT -u CLAUDE_CODE_SESSION_ID \
    -u CLAUDE_CODE_CHILD_SESSION -u CLAUDE_CODE_SESSION_ATTENDED -u CLAUDE_CODE_EXECPATH \
    -u CLAUDE_CODE_MESSAGING_SOCKET -u CLAUDE_CODE_MESSAGING_TOKEN -u CLAUDE_PID -u CLAUDE_EFFORT \
    -u CLAUDE_PLUGIN_DATA -u CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS \
    "$DRIVER" pty --cast "$C/s.cast" --cols 140 --rows 45 --cwd "$SP/proj" -- \
    "$CLAUDE" --model haiku --strict-mcp-config --mcp-config "$SP/mcp.json" \
    --debug-file "$C/debug.log" "$@" \
    --dangerously-load-development-channels server:s35probe > "$C/driver.out" 2> "$C/driver.err" &
  sleep 1
  cat "$FX/prelude.steps" "$T" >> "$C/steps"
  i=0; while [ $i -lt 150 ] && [ ! -e "$C/s.turn.txt" ]; do sleep 1; i=$((i+1)); done
  pkill -f "tail -f $C/steps"; sleep 1
  pkill -f "debug-file $C/debug.log"; pkill -f "demo-driver pty --cast $C/s.cast"
  wait
}

case_run a turn.steps  --settings "$(settings gate a)"
case_run e turn2.steps --settings "$(settings gate e)" --settings "$(settings op e)"
case_run f turn2.steps --settings "$(settings gate f '{"disableAllHooks":true}')"
case_run g turn2.steps --settings "$(settings gate g '{"allowManagedHooksOnly":true}')"
