#!/bin/sh
exec env DISABLE_AUTOUPDATER=1 ANTHROPIC_BASE_URL=http://127.0.0.1:8132 ANTHROPIC_AUTH_TOKEN=canned-placeholder ANTHROPIC_API_KEY= <HOME>/.local/bin/claude -p --output-format stream-json --verbose --setting-sources "" --strict-mcp-config "$@" -- "S38RO write the file"
