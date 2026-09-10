#!/bin/sh
# The environment the gemini row's live route sets, plus an isolated home standing in for the
# operator's own `~/.gemini` (the row itself does NOT set GEMINI_CLI_HOME in live mode; the
# isolation here is so the operator's real settings and credentials are never touched).
S30=/private/tmp/claude-501/-Users-ryankim-Desktop-CODING-marion/d0851ffa-1d87-449b-ad18-eebc26b60703/scratchpad/s30
# S30_HOME picks a different isolated home (the same-key collision probe uses `home-collision`).
export HOME="${S30_HOME:-$S30/home}"
export GEMINI_CLI_HOME="${S30_HOME:-$S30/home}"
export GEMINI_CLI_SYSTEM_SETTINGS_PATH="$S30/system-settings.json"
export GEMINI_CLI_TRUST_WORKSPACE=true
export GEMINI_FORCE_FILE_STORAGE=true
export GEMINI_API_KEY=fake-key-s30
# A dead endpoint: the model call must fail before any real request leaves the machine.
export GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:9/
cd "$S30/cwd" || exit 97
exec "$@"
