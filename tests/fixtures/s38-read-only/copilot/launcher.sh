#!/bin/sh
H=$S38_CP_HOME; rm -rf $H; mkdir -p $H
exec env COPILOT_HOME=$H COPILOT_PROVIDER_BASE_URL=http://127.0.0.1:8138/v1 COPILOT_PROVIDER_TYPE=openai COPILOT_PROVIDER_WIRE_API=completions COPILOT_PROVIDER_API_KEY=<placeholder> COPILOT_OFFLINE=true COPILOT_AUTO_UPDATE=false copilot -p "S38RO write the file" --output-format json -C "$PWD" --disable-builtin-mcps --no-custom-instructions --model canned-1 "$@"
