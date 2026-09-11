#!/bin/zsh
# S14 probe, codex half. Enumerates the tool surface codex exec declares under each --sandbox mode,
# and checks whether an unknown per-tool availability argument is an error.
set -u
# The workspace root: this file sits at <root>/tests/fixtures/s14/.
REPO=${REPO:-${0:A:h}/../../..}
OUT=${OUT:?}
mkdir -p "$OUT"
PORT=${PORT:-8733}
CWD=$(mktemp -d)

CH=$(mktemp -d)
cat > "$CH/config.toml" <<EOF
model_provider = "canned"
approval_policy = "never"

[features]
plugins = false

[model_providers.canned]
name = "canned"
base_url = "http://127.0.0.1:$PORT/v1"
wire_api = "responses"
env_key = "MARION_DUMMY_KEY"
EOF

run() {
  local tag=$1; shift
  local reqlog="$OUT/codex-$tag.reqlog.jsonl"
  rm -f "$reqlog"
  MARION_CANNED_PORT=$PORT MARION_CANNED_REQLOG="$reqlog" "$REPO/target/debug/canned" \
    >"$OUT/canned-codex-$tag.log" 2>&1 &
  local pid=$!
  sleep 1
  CODEX_HOME="$CH" MARION_DUMMY_KEY=marion-probe \
    codex exec --json --skip-git-repo-check "$@" -C "$CWD" "Say the single word OK." \
      >"$OUT/codex-$tag.stdout.jsonl" 2>"$OUT/codex-$tag.stderr.txt"
  echo "exit=$?" > "$OUT/codex-$tag.exit"
  sleep 1
  kill $pid 2>/dev/null
}

run readonly  --sandbox read-only
run wswrite   --sandbox workspace-write

# Pure argv: is an unknown per-tool availability flag an error?
codex exec --tools Read --skip-git-repo-check -C "$CWD" "x" \
  >"$OUT/codex-unknownflag.stdout.txt" 2>"$OUT/codex-unknownflag.stderr.txt"
echo "exit=$?" > "$OUT/codex-unknownflag.exit"
echo DONE
