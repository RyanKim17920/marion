#!/bin/sh
# $1 = sandbox mode; rest extra args
SB=$1; shift
H=$S38_CODEX_HOME; rm -rf $H; mkdir -p $H
cat > $H/config.toml <<T
model_provider = "canned"
approval_policy = "never"
sandbox_mode = "$SB"
check_for_update_on_startup = false
[features]
plugins = false
[model_providers.canned]
name = "canned"
base_url = "http://127.0.0.1:8133/v1"
wire_api = "responses"
env_key = "MARION_PROVIDER_KEY"
T
exec env CODEX_HOME=$H MARION_PROVIDER_KEY=canned-placeholder <HOME>/.local/bin/codex exec -C "$PWD" --json --skip-git-repo-check -c check_for_update_on_startup=false "$@" "S38RO write the file"
