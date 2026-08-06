#!/bin/zsh
# S14 probe, claude half. Measures what `--tools` does to the *availability* axis, by reading the
# tool declarations off the request body the CLI actually sends to a canned local endpoint.
# No vendor endpoint, no key, no token.
set -u
REPO=${REPO:-/Users/ryankim/Desktop/CODING/marion}
OUT=${OUT:-$REPO/../s14-out}
mkdir -p "$OUT"
PORT=${PORT:-8731}
CWD=$(mktemp -d)

"$REPO/target/debug/canned" >"$OUT/canned.log" 2>&1 &
sleep 0
CANNED_PID=$!

run() {
  local tag=$1; shift
  local reqlog="$OUT/claude-$tag.reqlog.jsonl"
  rm -f "$reqlog"
  MARION_CANNED_PORT=$PORT MARION_CANNED_REQLOG="$reqlog" "$REPO/target/debug/canned" \
    >"$OUT/canned-$tag.log" 2>&1 &
  local pid=$!
  sleep 1
  printf '%s\n' '{"type":"user","session_id":"","message":{"role":"user","content":[{"type":"text","text":"Say the single word OK."}]},"parent_tool_use_id":null}' \
    | ANTHROPIC_BASE_URL="http://127.0.0.1:$PORT" ANTHROPIC_AUTH_TOKEN=marion-probe ANTHROPIC_API_KEY= \
      claude -p --output-format stream-json --input-format stream-json --verbose \
        --setting-sources "" --model haiku "$@" \
        >"$OUT/claude-$tag.stdout.jsonl" 2>"$OUT/claude-$tag.stderr.txt"
  echo "exit=$?" > "$OUT/claude-$tag.exit"
  sleep 1
  kill $pid 2>/dev/null
}

kill $CANNED_PID 2>/dev/null

run empty        --tools "" --allowedTools ""
run read         --tools "Read" --allowedTools "Read"
run bogus        --tools "NotATool" --allowedTools ""
run readplusbogus --tools "Read,NotATool" --allowedTools "Read"
run absent       --allowedTools ""
run default        --tools "default" --allowedTools ""
run readbash       --tools "Read,Bash" --allowedTools "Read,Bash"
run lowercase      --tools "read" --allowedTools "read"
echo DONE
