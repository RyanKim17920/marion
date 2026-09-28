#!/bin/sh
# usage: qw [--yolo] -- <core tools...>
H=$S38_QW_HOME; rm -rf $H; mkdir -p $H
echo '{"memory":{"enableManagedAutoMemory":false}}' > $H/settings.json
Y=""; [ "$1" = "--yolo" ] && { Y="--yolo"; shift; }
exec env QWEN_HOME=$H OPENAI_BASE_URL=http://127.0.0.1:8136/v1 OPENAI_API_KEY=canned-placeholder OPENAI_MODEL=canned-1 QWEN_CODE_LEGACY_MCP_BLOCKING=1 QWEN_CODE_SKIP_UPDATE_CHECK_ONCE=true NO_COLOR=1 TERM=dumb qwen $Y --core-tools "$@" --exclude-tools agent enter_worktree exit_worktree get_goal list_agents record_artifact report_findings send_message skill task_stop tool_search update_goal -p "S38RO write the file" --output-format stream-json
