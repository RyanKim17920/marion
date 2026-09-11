#!/bin/zsh
# S14 probe, opencode half. Canned OpenAI-compatible provider per S13's measured config; reads the
# declared tool list off the chat/completions request body under each `permission` setting.
set -u
# The workspace root: this file sits at <root>/tests/fixtures/s14/.
REPO=${REPO:-${0:A:h}/../../..}
OUT=${OUT:?}
mkdir -p "$OUT"
PORT=${PORT:-8737}
CWD=$(mktemp -d)
HOMEDIR=$(mktemp -d)

CFG='{"model":"fake/fake-1","small_model":"fake/fake-1","provider":{"fake":{"npm":"@ai-sdk/openai-compatible","name":"Fake","options":{"baseURL":"http://127.0.0.1:'$PORT'/v1","apiKey":"sk-fake"},"models":{"fake-1":{"name":"Fake One","tool_call":true}}}}}'

run() {
  local tag=$1; local perm=$2
  local reqlog="$OUT/opencode-$tag.reqlog.jsonl"
  rm -f "$reqlog"
  MARION_CANNED_PORT=$PORT MARION_CANNED_REQLOG="$reqlog" "$REPO/target/debug/canned" \
    >"$OUT/canned-opencode-$tag.log" 2>&1 &
  local pid=$!
  sleep 1
  local -a envargs
  if [[ -n "$perm" ]]; then
    HOME="$HOMEDIR" XDG_CONFIG_HOME="$HOMEDIR/.config" PWD="$CWD" \
      OPENCODE_DISABLE_MODELS_FETCH=1 OPENCODE_CONFIG_CONTENT="$CFG" OPENCODE_PERMISSION="$perm" \
      timeout 25 opencode run --pure --format json -m fake/fake-1 "Say the single word OK." \
        >"$OUT/opencode-$tag.stdout.jsonl" 2>"$OUT/opencode-$tag.stderr.txt"
  else
    HOME="$HOMEDIR" XDG_CONFIG_HOME="$HOMEDIR/.config" PWD="$CWD" \
      OPENCODE_DISABLE_MODELS_FETCH=1 OPENCODE_CONFIG_CONTENT="$CFG" \
      timeout 25 opencode run --pure --format json -m fake/fake-1 "Say the single word OK." \
        >"$OUT/opencode-$tag.stdout.jsonl" 2>"$OUT/opencode-$tag.stderr.txt"
  fi
  echo "exit=$? (124 = the 25s bound; opencode does not exit on a script it cannot satisfy, S13)" > "$OUT/opencode-$tag.exit"
  kill $pid 2>/dev/null
  # Keep only the first 4 MB: the loop grows the log without bound and adds nothing new.
  head -c 4000000 "$reqlog" > "$reqlog.head" && mv "$reqlog.head" "$reqlog"
}

run default   ''
run readdeny  '{"read":"deny"}'
run readallow '{"read":"allow"}'
run bogusdeny '{"NotATool":"deny"}'
echo DONE
