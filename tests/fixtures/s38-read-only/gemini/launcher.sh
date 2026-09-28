#!/bin/sh
H=$S38_GM_HOME; rm -rf $H; mkdir -p $H
cat > $H/marion-settings.json <<T
{"privacy":{"usageStatisticsEnabled":false},"security":{"auth":{"selectedType":"gemini-api-key"}},"general":{"enableAutoUpdate":false,"enableAutoUpdateNotification":false}}
T
exec env GEMINI_CLI_HOME=$H GEMINI_CLI_SYSTEM_SETTINGS_PATH=$H/marion-settings.json GEMINI_CLI_TRUST_WORKSPACE=true GEMINI_FORCE_FILE_STORAGE=true GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:8135 GEMINI_API_KEY=canned-placeholder gemini -m gemini-2.5-flash --output-format stream-json "$@" -p "S38RO write the file"
