# source: mkenv.sh <probe-dir> <port> [extra-json-merged-into-config]
D=$1; P=$2; EXTRA=${3:-{\}}
rm -rf $D; mkdir -p $D; cp -R sb-warm $D/sb; SB=$D/sb
python3 - "$SB/config/opencode/opencode.json" "$P" "$PWD/slowmcp.py" "$D/mcp.jsonl" "$EXTRA" <<'PY'
import json, sys
path, port, slow, log, extra = sys.argv[1:]
c = {"model": "canned/canned-1", "small_model": "canned/canned-1",
     "provider": {"canned": {"npm": "@ai-sdk/openai-compatible", "name": "canned",
        "options": {"baseURL": f"http://127.0.0.1:{port}/v1", "apiKey": "sk-fake", "timeout": 60000},
        "models": {"canned-1": {"name": "canned-1", "tool_call": True}}}},
     "mcp": {"slow": {"type": "local", "command": ["python3", slow], "environment": {"SLOW_LOG": log, "SLOW_DELAY": "8"}, "enabled": True}}}
def merge(a, b):
    for k, v in b.items():
        if isinstance(v, dict) and isinstance(a.get(k), dict): merge(a[k], v)
        else: a[k] = v
merge(c, json.loads(extra))
open(path, "w").write(json.dumps(c))
PY
export HOME=$SB XDG_CONFIG_HOME=$SB/config XDG_DATA_HOME=$SB/data XDG_CACHE_HOME=$SB/cache XDG_STATE_HOME=$SB/state OPENCODE_DISABLE_AUTOUPDATE=1 OPENCODE_DISABLE_MODELS_FETCH=1 OPENCODE_DISABLE_LSP_DOWNLOAD=1 OPENCODE_DISABLE_SHARE=1 OPENCODE_DISABLE_CLAUDE_CODE=1 OPENCODE_DISABLE_EXTERNAL_SKILLS=1 OPENCODE_DISABLE_PROJECT_CONFIG=1 OPENCODE_DB=$SB/oc.db
