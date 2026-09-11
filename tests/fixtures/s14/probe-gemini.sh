#!/bin/zsh
# S14 probe, gemini half. Uses S12's technique: a throwaway GEMINI_API_KEY with
# GOOGLE_GEMINI_BASE_URL pointed at the canned server, so no Google endpoint is contacted and the
# IneligibleTier block on the personal login is never reached.
set -u
# The workspace root: this file sits at <root>/tests/fixtures/s14/.
REPO=${REPO:-${0:A:h}/../../..}
OUT=${OUT:?}
mkdir -p "$OUT"
PORT=${PORT:-8735}
CWD=$(mktemp -d)
HOMEDIR=$(mktemp -d)
cat > "$HOMEDIR/settings.json" <<'EOF'
{"security":{"auth":{"selectedType":"gemini-api-key"}},"general":{"enableAutoUpdate":false,"enableAutoUpdateNotification":false},"privacy":{"usageStatisticsEnabled":false}}
EOF

run() {
  local tag=$1; shift
  local reqlog="$OUT/gemini-$tag.reqlog.jsonl"
  rm -f "$reqlog"
  MARION_CANNED_PORT=$PORT MARION_CANNED_REQLOG="$reqlog" "$REPO/target/debug/canned" \
    >"$OUT/canned-gemini-$tag.log" 2>&1 &
  local pid=$!
  sleep 1
  ( cd "$CWD" && \
    GEMINI_CLI_HOME="$HOMEDIR" \
    GEMINI_CLI_SYSTEM_SETTINGS_PATH="$HOMEDIR/settings.json" \
    GEMINI_CLI_TRUST_WORKSPACE=true \
    GEMINI_FORCE_FILE_STORAGE=true \
    GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:$PORT" \
    GEMINI_API_KEY=marion-probe \
    gemini -m gemini-2.5-flash --output-format stream-json "$@" -p "Say the single word OK." \
      >"$OUT/gemini-$tag.stdout.jsonl" 2>"$OUT/gemini-$tag.stderr.txt" )
  echo "exit=$?" > "$OUT/gemini-$tag.exit"
  sleep 1
  kill $pid 2>/dev/null
}

run default
run allowed-read     --allowed-tools read_file
run allowed-bogus    --allowed-tools NotATool
run planmode         --approval-mode plan
echo DONE
