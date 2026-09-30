#!/usr/bin/env bash
# One admission run of marion's node sandbox on the operator's own login, for one harness row.
#
# A row covers the operator's own login only once one live task has run under its profile and
# shown that its `Live` list (crates/marion-harness/src/<row>.rs) is enough. This runs that task:
# a scratch git repo, one headless `marion run <harness> --prompt "create hello.txt with one line"`
# on the operator's existing login with MARION_SANDBOX_ADMIT=1 (the node runs under the profile
# without being counted contained), and a before/after mtime diff of $HOME. Its result, a JSON file,
# is what scripts/sandbox-admit-apply.py turns into the row's `admitted` note.
#
#   scripts/sandbox-admit.sh claude            # print the plan and the expected writes; runs nothing
#   MARION_SANDBOX_ADMIT_RUN=1 scripts/sandbox-admit.sh claude
#                                              # the live run: SPENDS a small real-model task
#
# It never starts a login, never reads or copies a credential, and never relocates HOME. A harness
# whose own status probe says it is logged out is BLOCKED. Every launch carries the harness's
# no-self-update switch (marion's rows add it too). Before a live run, back up what the run could
# touch yourself — the harness's session/state dirs listed as `dir` below — if you want to restore
# them; this script does not copy them, because they sit beside credential files.
#
# Harnesses record the folders they run in (claude in ~/.claude.json, codex in ~/.codex/config.toml):
# under the profile those writes are refused, so their absence from the diff is expected, and a
# harness that fails without them has an incomplete list — which is what the run is for.

set -euo pipefail

H=${1:?usage: scripts/sandbox-admit.sh <claude|codex|opencode|pi>}
REPO_ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
TASK="create hello.txt with one line"
DATE=$(date +%Y-%m-%d)
OUT=${MARION_SANDBOX_ADMIT_OUT:-$REPO_ROOT/target/sandbox-admit}
WALL_SECS=${MARION_SANDBOX_ADMIT_WALL_SECS:-300}

case "$H" in
	claude) export DISABLE_AUTOUPDATER=1 ;;
	codex) : ;; # marion's codex row carries `-c check_for_update_on_startup=false` on every launch
	opencode) export OPENCODE_DISABLE_AUTOUPDATE=1 ;;
	pi) export PI_SKIP_VERSION_CHECK=1 ;;
	*) echo "sandbox-admit: no admission recipe for \`$H\` (claude, codex, opencode, pi)" >&2; exit 2 ;;
esac

logged_in() {
	case "$H" in
		claude) claude auth status --json 2>/dev/null | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("loggedIn") else 1)' ;;
		codex) codex -c check_for_update_on_startup=false login status >/dev/null 2>&1 ;;
		# Existence only: the file is never opened.
		opencode) [ -s "${XDG_DATA_HOME:-$HOME/.local/share}/opencode/auth.json" ] ;;
		pi) [ -s "$HOME/.pi/agent/auth.json" ] ;;
	esac
}

(cd "$REPO_ROOT" && cargo build -q --bin marion --bin marion-supervisor)
BIN="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/debug"
SCRATCH=$(mktemp -d "${TMPDIR:-/tmp}/marion-admit-$H.XXXXXX")
SCRATCH=$(cd "$SCRATCH" && pwd -P)
REPO="$SCRATCH/repo"
mkdir -p "$REPO" "$OUT"
git -C "$REPO" init -q
git -C "$REPO" -c user.name=admit -c user.email=admit@localhost commit -q --allow-empty -m init

echo "sandbox-admit: $H $("$H" --version 2>&1 | head -1)"
echo "sandbox-admit: what the profile lets the node write beyond its own dirs (repo: $REPO):"
"$BIN/marion-supervisor" sandbox-plan "$H" --cwd "$REPO" | sed 's/^/  /'

if [ "${MARION_SANDBOX_ADMIT_RUN:-}" != "1" ]; then
	echo "sandbox-admit: plan only. Set MARION_SANDBOX_ADMIT_RUN=1 to run the live task (a small real-model spend)."
	rm -rf "$SCRATCH"
	exit 0
fi
if ! logged_in; then
	echo "sandbox-admit: $H BLOCKED: not logged in (log in with its own CLI first; this script never does)" >&2
	printf '{"harness":"%s","blocked":"not logged in"}\n' "$H" >"$OUT/$H.json"
	exit 0
fi

MARK="$SCRATCH/mark"
touch "$MARK"
sleep 1
set +e
(cd "$REPO" && MARION_SANDBOX_ADMIT=1 timeout "$WALL_SECS" "$BIN/marion" run "$H" \
	--prompt "$TASK" --state-dir "$SCRATCH/state" --timeout "$WALL_SECS") >"$SCRATCH/run.out" 2>"$SCRATCH/run.err"
EXIT=$?
set -e

# Every file under $HOME (and the per-user /tmp roots rows name) written since the mark.
CHANGED="$SCRATCH/changed.txt"
{
	find "$HOME" -xdev -newer "$MARK" -type f 2>/dev/null
	find "/tmp/$H-$(id -u)" -newer "$MARK" -type f 2>/dev/null || true
} | sed "s#^/tmp/#/private/tmp/#" | grep -v "^$SCRATCH/" >"$CHANGED" || true

"$BIN/marion-supervisor" sandbox-plan "$H" --cwd "$REPO" >"$SCRATCH/plan.txt"
python3 - "$H" "$("$H" --version 2>&1 | head -1)" "$DATE" "$TASK" "$EXIT" "$REPO" "$SCRATCH/plan.txt" "$CHANGED" "$SCRATCH/run.err" >"$OUT/$H.json" <<'PY'
import json, os, sys
h, version, date, task, code, repo, plan, changed, err = sys.argv[1:]
real = lambda p: os.path.realpath(p)
dirs, files, admitted = [], [], None
for line in open(plan):
    kind, _, path = line.rstrip("\n").partition(" ")
    if kind == "dir":
        dirs.append(real(path))
    elif kind == "file":
        files.append(real(path))
    elif kind == "admitted":
        admitted = path
within, outside = [], []
for p in (l.rstrip("\n") for l in open(changed) if l.strip()):
    rp = real(p)
    ok = rp in files or any(rp == d or rp.startswith(d + os.sep) for d in dirs)
    (within if ok else outside).append(p)
print(json.dumps({
    "harness": h, "version": version, "date": date, "task": task,
    "exit": int(code), "hello": os.path.isfile(os.path.join(repo, "hello.txt")),
    "within": within, "outside": outside, "was_admitted": admitted,
    "stderr_tail": open(err, errors="replace").read()[-2000:],
}, indent=2))
PY
echo "sandbox-admit: result in $OUT/$H.json"
python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); print(f"  exit {r[\"exit\"]}, hello.txt {r[\"hello\"]}, {len(r[\"within\"])} writes inside the list, {len(r[\"outside\"])} outside:"); [print("   ", p) for p in r["outside"]]' "$OUT/$H.json"
rm -rf "$SCRATCH"
