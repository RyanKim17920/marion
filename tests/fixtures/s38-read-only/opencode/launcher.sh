#!/bin/sh
PERM=""; [ -n "$S38_OC_PERM" ] && PERM=",\"permission\":$S38_OC_PERM"
H=$S38_OC_HOME; rm -rf $H; mkdir -p $H/xdg/opencode $H/data $H/cache $H/state
cat > $H/xdg/opencode/opencode.json <<T
{"model":"canned/canned-1","small_model":"canned/canned-1","provider":{"canned":{"npm":"@ai-sdk/openai-compatible","name":"canned","options":{"baseURL":"http://127.0.0.1:8134/v1","apiKey":"canned-placeholder","timeout":30000},"models":{"canned-1":{"name":"canned-1","tool_call":true}}}}$PERM}
T
exec env HOME=$H XDG_CONFIG_HOME=$H/xdg XDG_DATA_HOME=$H/data XDG_CACHE_HOME=$H/cache XDG_STATE_HOME=$H/state OPENCODE_DISABLE_CLAUDE_CODE=1 OPENCODE_DISABLE_EXTERNAL_SKILLS=1 OPENCODE_DISABLE_PROJECT_CONFIG=1 OPENCODE_DISABLE_MODELS_FETCH=1 OPENCODE_DISABLE_LSP_DOWNLOAD=1 OPENCODE_DISABLE_SHARE=1 OPENCODE_DISABLE_AUTOUPDATE=1 OPENCODE_DB=$H/session.db PWD="$PWD" opencode run --pure --format json --title s38 -m canned/canned-1 "S38RO write the file"
