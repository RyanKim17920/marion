#!/bin/sh
# S8 -- design §11 item 3: does CODEX_HOME isolation break Codex auth?
#
# Re-runnable measurement. Uses `codex login status`, which reports auth state
# WITHOUT making a model call, so cases A-E cost nothing. Case F optionally
# makes exactly one trivial real call.
#
# SAFETY: this script reads ~/.codex/auth.json and copies it into a scratch
# dir. It never prints the contents, and it never writes to ~/.codex. Run it
# with WORK pointed at a scratch path outside the repo, or rely on the
# .gitignore next to this file.
set -u
WORK="${WORK:-${TMPDIR:-/tmp}/marion-s8}"
REAL_AUTH="$HOME/.codex/auth.json"

red() { sed -E 's/(ey[A-Za-z0-9_-]{10,}|sk-[A-Za-z0-9_-]{10,})/<REDACTED>/g; s/[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+/<EMAIL>/g'; }
case_() { printf '\n=== %s ===\n' "$1"; }

rm -rf "$WORK"; mkdir -p "$WORK"

case_ "A: baseline, real CODEX_HOME inherited"
codex login status 2>&1 | red

case_ "B: isolated fresh empty CODEX_HOME"
mkdir -p "$WORK/b"
CODEX_HOME="$WORK/b" codex login status 2>&1 | red

case_ "C: isolated CODEX_HOME + copied auth.json"
mkdir -p "$WORK/c"; chmod 700 "$WORK/c"; cp "$REAL_AUTH" "$WORK/c/auth.json"
CODEX_HOME="$WORK/c" codex login status 2>&1 | red

case_ "D: isolated CODEX_HOME, auth.json is a SYMLINK to the copy"
mkdir -p "$WORK/d"; ln -sf "$WORK/c/auth.json" "$WORK/d/auth.json"
CODEX_HOME="$WORK/d" codex login status 2>&1 | red

case_ "E: isolated CODEX_HOME + OPENAI_API_KEY / CODEX_AUTH env, no files"
mkdir -p "$WORK/e"
CODEX_HOME="$WORK/e" OPENAI_API_KEY=dummy-not-real codex login status 2>&1 | red
CODEX_HOME="$WORK/e" CODEX_AUTH="$(cat "$REAL_AUTH")" codex login status 2>&1 | red

case_ "keychain: does Codex store credentials there? (existence only, no values)"
for svc in codex Codex codex-cli OpenAI openai com.openai.codex ChatGPT; do
  if security find-generic-password -s "$svc" >/dev/null 2>&1; then
    echo "FOUND service=$svc"
  else
    echo "absent service=$svc"
  fi
done
# Control: Claude Code IS keychain-backed, so this one must be FOUND.
security find-generic-password -s "Claude Code-credentials" >/dev/null 2>&1 \
  && echo "control: FOUND service=Claude Code-credentials" \
  || echo "control: absent (unexpected)"

if [ "${S8_REAL_CALL:-0}" = "1" ]; then
  case_ "F: ONE real model call under isolated CODEX_HOME + copied auth.json"
  mkdir -p "$WORK/wt"
  # NB: do not pin a `model` here -- an unsupported model yields a 400 that
  # looks like a failure but is not an auth failure.
  CODEX_HOME="$WORK/c" codex exec --skip-git-repo-check -C "$WORK/wt" \
    "Reply with exactly the word: pong" 2>&1 | tail -5 | red
fi

case_ "post-check: real auth.json must be byte-identical"
shasum -a 256 "$REAL_AUTH" | cut -c1-16
codex login status 2>&1 | red
