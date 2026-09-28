#!/bin/sh
# usage: gs <mode> [extra args]
M=$1; shift
H=$S38_GS_HOME; rm -rf $H; mkdir -p $H
exec env HOME=$H GOOSE_PROVIDER=openai GOOSE_MODEL=canned-1 OPENAI_HOST=http://127.0.0.1:8137/v1 OPENAI_BASE_PATH=chat/completions OPENAI_API_KEY=canned-placeholder GOOSE_MODE=$M goose run -t "S38RO write the file" --output-format stream-json -q --no-session --no-profile "$@"
