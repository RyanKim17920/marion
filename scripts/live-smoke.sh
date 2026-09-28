#!/usr/bin/env bash
# marion's live smoke suite: real roots, real children, real models, on the operator's own logins.
#
# Every other end-to-end test drives a canned provider whose scripted "model" makes one or two fixed
# tool calls. This suite is the only place a real model decides, from a plain request, to delegate
# through marion's `spawn`, writes code a real test suite has to pass, reports on its own, and takes
# an operator's steer. Each scenario gets a fresh fixture repository (a tiny Python module and its
# unittest suite) in a scratch dir, runs one headless root with `marion run`, and is judged by the
# fixture's real test command plus a scenario check, run on the branch the child landed.
#
#   s1  claude root -> codex child     add word_count(s) with tests
#   s2  codex root  -> claude child    add char_frequency(s) with tests
#   s3  claude root -> opencode child  fix an off-by-one it has to read the file to find
#   s4  claude root -> codex child     add average(nums); the operator steers the running child
#                                      with "also handle empty input" (`marion steer`)
#
# Usage:
#   MARION_LIVE_SMOKE=1 scripts/live-smoke.sh            # all four, once
#   MARION_LIVE_SMOKE=1 scripts/live-smoke.sh s3         # one scenario (an infrastructure rerun)
#   MARION_LIVE_SMOKE=1 MARION_LIVE_SMOKE_CANNED=1 MARION_LIVE_SMOKE_OUT=/tmp/x scripts/live-smoke.sh s1
#                                  # the driver's own plumbing against marion-canned: free, and the
#                                  # canned script ignores the prompt, so every verdict is a fail
#
# SPENDS REAL MONEY on the operator's own logins, so it is gated on MARION_LIVE_SMOKE=1, is never run
# by CI, and keeps every task tiny: claude roots run on haiku, other harnesses on their own default.
# It never starts a login, never relocates HOME and never reads a credential; a harness whose own
# status probe says it is logged out is BLOCKED and skipped. Status probes carry each harness's
# no-self-update switch, as marion's launches do.
#
# Claude records every folder it runs in under `projects` in ~/.claude.json. The suite backs that
# file up once (0600, in the run's scratch dir) and afterwards deletes exactly the entries under the
# run's scratch root, nothing else. Results and redacted transcripts go to
# tests/fixtures/live-smoke-<date>/ (override with MARION_LIVE_SMOKE_OUT); read them against
# tests/fixtures/REVIEW.md before committing.

set -euo pipefail

if [ "${MARION_LIVE_SMOKE:-}" != "1" ]; then
	echo "SKIPPED live-smoke: set MARION_LIVE_SMOKE=1 to run real models on your own logins (costs money)" >&2
	exit 0
fi

REPO_ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
COLLECT="$REPO_ROOT/scripts/live-smoke-collect.py"
# The wall clock one scenario gets, root and children together.
WALL_SECS=${MARION_LIVE_SMOKE_WALL_SECS:-600}
DATE=$(date +%Y-%m-%d)
OUT=${MARION_LIVE_SMOKE_OUT:-$REPO_ROOT/tests/fixtures/live-smoke-$DATE}
ONLY=${1:-}
CANNED=${MARION_LIVE_SMOKE_CANNED:+1}

export CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only

for tool in git python3 timeout; do
	command -v "$tool" >/dev/null || { echo "live-smoke: needs $tool on PATH" >&2; exit 2; }
done

if [ -n "${MARION_BIN:-}" ]; then
	BIN=$MARION_BIN
else
	echo "live-smoke: building marion" >&2
	cargo build --quiet --manifest-path "$REPO_ROOT/Cargo.toml" -p marion-supervisor --bins
	BIN="$REPO_ROOT/target/debug/marion"
fi

RUN=$(mktemp -d "${TMPDIR:-/tmp}/marion-live-smoke.XXXXXX")
RUN=$(cd "$RUN" && pwd -P)
mkdir -p "$OUT"

# ---- ~/.claude.json: one backup, then only this run's own `projects` entries removed -------------

CLAUDE_JSON="$HOME/.claude.json"
if [ -f "$CLAUDE_JSON" ]; then
	(umask 077 && cp -p "$CLAUDE_JSON" "$RUN/claude.json.bak")
fi

prune_claude_json() {
	[ -f "$CLAUDE_JSON" ] || return 0
	python3 - "$CLAUDE_JSON" "$RUN" <<-'EOF'
		import json, os, sys, tempfile
		path, scratch = sys.argv[1], sys.argv[2]
		prefixes = {scratch, os.path.realpath(scratch)}
		if scratch.startswith("/private/"):
		    prefixes.add(scratch[len("/private"):])
		with open(path) as f:
		    doc = json.load(f)
		projects = doc.get("projects") or {}
		gone = [k for k in projects if any(k == p or k.startswith(p + "/") for p in prefixes)]
		if not gone:
		    sys.exit(0)
		for k in gone:
		    del projects[k]
		fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path), prefix=".claude.json.live-smoke.")
		with os.fdopen(fd, "w") as f:
		    json.dump(doc, f, indent=2)
		os.chmod(tmp, os.stat(path).st_mode & 0o777)
		os.replace(tmp, path)
		print(f"live-smoke: removed {len(gone)} ~/.claude.json project entries under the scratch root", file=sys.stderr)
	EOF
}

cleanup() {
	prune_claude_json || echo "live-smoke: could not prune ~/.claude.json; backup at $RUN/claude.json.bak" >&2
	# Worktrees a killed child left behind point into $RUN; drop the scratch repos with them.
	find "$RUN" -mindepth 1 -maxdepth 1 -type d -name 's*' -exec rm -rf {} + 2>/dev/null || true
	echo "live-smoke: scratch kept for inspection at $RUN (holds only the ~/.claude.json backup)" >&2
}
trap cleanup EXIT

# ---- login status, read with each harness's own probe, never a login ----------------------------

logged_in() {
	case "$1" in
		claude) DISABLE_AUTOUPDATER=1 claude auth status --json 2>/dev/null | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("loggedIn") else 1)' ;;
		codex) codex -c check_for_update_on_startup=false login status >/dev/null 2>&1 ;;
		# opencode's status probe is a file (the README's profile table); no binary launch at all.
		opencode) [ -s "${XDG_DATA_HOME:-$HOME/.local/share}/opencode/auth.json" ] ;;
		*) return 1 ;;
	esac
}

# ---- the fixture ------------------------------------------------------------------------------------

make_fixture() {
	local repo=$1
	mkdir -p "$repo"
	cat >"$repo/textutil.py" <<-'EOF'
		"""Small text helpers."""


		def count_lines(s):
		    """Return the number of lines in s."""
		    return len(s.splitlines())


		def last_n_lines(s, n):
		    """Return the last n lines of s, oldest first."""
		    lines = s.splitlines()
		    return lines[len(lines) - n - 1:]
	EOF
	cat >"$repo/test_textutil.py" <<-'EOF'
		import unittest

		from textutil import count_lines


		class CountLinesTest(unittest.TestCase):
		    def test_counts_lines(self):
		        self.assertEqual(count_lines("a\nb\nc"), 3)

		    def test_empty(self):
		        self.assertEqual(count_lines(""), 0)


		if __name__ == "__main__":
		    unittest.main()
	EOF
	printf '__pycache__/\n' >"$repo/.gitignore"
	git -C "$repo" init -q -b main
	git -C "$repo" add -A
	git -C "$repo" -c user.name=live-smoke -c user.email=live-smoke@example.invalid commit -qm "fixture"
}

TEST_CMD="python3 -m unittest -q"

# ---- one scenario -----------------------------------------------------------------------------------

# scenario <id> <root harness> <root model|-> <child harness> <prompt> <check> [<steer text>]
scenario() {
	local id=$1 root=$2 model=$3 child=$4 prompt=$5 check=$6 steer=${7:-}
	if [ -n "$ONLY" ] && [ "$ONLY" != "$id" ]; then
		return 0
	fi
	local dir="$RUN/$id" out="$OUT/$id"
	mkdir -p "$dir/state" "$out"
	for h in "$root" "$child"; do
		if [ -z "$CANNED" ] && ! logged_in "$h"; then
			echo "live-smoke: $id BLOCKED: $h is not logged in (log in with its own CLI, then rerun $id)" >&2
			printf '{"scenario":"%s","blocked":"%s not logged in"}\n' "$id" "$h" >"$out/result.json"
			return 0
		fi
	done
	make_fixture "$dir/repo"

	local args=(run "$root" --prompt "$prompt" --repo "$dir/repo" --state-dir "$dir/state" --timeout "$WALL_SECS")
	[ "$model" != "-" ] && args+=(--model "$model")
	[ -n "$CANNED" ] && args+=(--canned)

	echo "live-smoke: $id: $root root -> $child child (wall ${WALL_SECS}s)" >&2
	local start end rc=0 timed_out=no
	start=$(date +%s)
	(cd "$dir/repo" && exec timeout -k 15 "$WALL_SECS" "$BIN" "${args[@]}") >"$dir/run.out" 2>&1 &
	local run_pid=$!

	if [ -n "$steer" ]; then
		steer_first_child "$dir" "$steer" "$run_pid" "$start" >"$dir/steer.out" 2>&1 || true
	fi
	wait "$run_pid" || rc=$?
	end=$(date +%s)
	[ "$rc" -eq 124 ] || [ "$rc" -eq 137 ] && timed_out=yes
	[ "$timed_out" = yes ] && stop_leftovers "$dir"

	python3 "$COLLECT" --state "$dir/state" --scratch "$RUN" --scenario "$id" --expect-child "$child" \
		--repo "$dir/repo" --test-cmd "$TEST_CMD" --check "$check" --wall-secs $((end - start)) \
		--timed-out "$timed_out" --steer "$steer" --console "$dir/run.out" --steer-log "$dir/steer.out" \
		--out "$out" >/dev/null ||
		echo "live-smoke: $id: collecting results failed (see $dir)" >&2
	echo "live-smoke: $id done in $((end - start))s (marion run exit $rc)" >&2
	prune_claude_json
}

# Wait for the root's first child to be running, then steer it as the operator.
steer_first_child() {
	local dir=$1 text=$2 run_pid=$3 start=$4 child=""
	while kill -0 "$run_pid" 2>/dev/null; do
		child=$(python3 - "$dir/state" <<-'EOF'
			import json, pathlib, sys
			children, running = set(), []
			for j in pathlib.Path(sys.argv[1]).glob("*/journal.jsonl"):
			    for line in j.read_text(errors="replace").splitlines():
			        try:
			            k = json.loads(line).get("kind")
			        except json.JSONDecodeError:
			            continue
			        if not isinstance(k, dict):
			            continue
			        if "SpawnIntent" in k and k["SpawnIntent"].get("parent_id"):
			            children.add(k["SpawnIntent"]["agent_id"])
			        elif "Spawned" in k and k["Spawned"]["agent_id"] in children:
			            running.append(k["Spawned"]["agent_id"])
			if running:
			    print(running[0])
		EOF
		)
		[ -n "$child" ] && break
		sleep 2
	done
	[ -n "$child" ] || { echo "no child was running before the root ended"; return 1; }
	# A few seconds into its first turn, so the steer lands mid-task rather than ahead of it.
	sleep 5
	echo "steering $child at +$(($(date +%s) - start))s: $text"
	(cd "$dir/repo" && "$BIN" steer "$child" --state-dir "$dir/state" "$text")
}

# A scenario killed at its wall clock: cancel whatever marion still runs for it.
stop_leftovers() {
	local dir=$1
	python3 - "$dir/state" <<-'EOF' | while read -r id; do (cd "$dir/repo" && "$BIN" cancel "$id" --state-dir "$dir/state") || true; done
		import json, pathlib, sys
		live = {}
		for j in pathlib.Path(sys.argv[1]).glob("*/journal.jsonl"):
		    for line in j.read_text(errors="replace").splitlines():
		        k = json.loads(line).get("kind")
		        if isinstance(k, dict) and "SpawnIntent" in k:
		            live[k["SpawnIntent"]["agent_id"]] = True
		        elif isinstance(k, dict) and "Exited" in k:
		            live.pop(k["Exited"]["agent_id"], None)
		print("\n".join(live))
	EOF
}

# ---- the scenarios ----------------------------------------------------------------------------------

scenario s1 claude haiku codex \
	"Delegate this to a codex agent: add a word_count(s) function to textutil.py that returns the number of whitespace-separated words in s, with unit tests in test_textutil.py. The project's test command is \`$TEST_CMD\`." \
	'from textutil import word_count as w; assert w("a  b\nc") == 3, w("a  b\nc"); assert w("") == 0; assert w("   ") == 0'

scenario s2 codex - claude \
	"Delegate this to a claude agent on the haiku model: add a char_frequency(s) function to textutil.py that returns a dict mapping each character of s to how many times it occurs, with unit tests in test_textutil.py. The project's test command is \`$TEST_CMD\`." \
	'from textutil import char_frequency as f; assert f("aab") == {"a": 2, "b": 1}, f("aab"); assert f("") == {}'

scenario s3 claude haiku opencode \
	"Delegate this to an opencode agent: last_n_lines in textutil.py has an off-by-one bug. Fix it and add a regression test to test_textutil.py. The project's test command is \`$TEST_CMD\`." \
	'from textutil import last_n_lines as l; assert l("a\nb\nc", 2) == ["b", "c"], l("a\nb\nc", 2); assert l("a\nb\nc", 1) == ["c"]; assert l("a\nb\nc", 3) == ["a", "b", "c"]'

scenario s4 claude haiku codex \
	"Delegate this to a codex agent: add an average(nums) function to textutil.py that returns the arithmetic mean of a list of numbers, with unit tests in test_textutil.py. The project's test command is \`$TEST_CMD\`." \
	'from textutil import average as a; assert a([1, 2, 3]) == 2; assert a([]) == 0.0, "empty input was not handled"' \
	"Also handle empty input: average([]) must return 0.0 instead of raising. Add a test for it."

# ---- the table --------------------------------------------------------------------------------------

python3 - "$OUT" <<-'EOF' | tee "$OUT/results.md"
	import json, pathlib, sys
	out = pathlib.Path(sys.argv[1])
	print("| scenario | delegated | child (harness, model) | reported | verify on branch | landed | wall | tokens root/child | steer |")
	print("|---|---|---|---|---|---|---|---|---|")
	yn = lambda b: "y" if b else "n"
	for p in sorted(out.glob("s*/result.json")):
	    r = json.loads(p.read_text())
	    if "blocked" in r:
	        print(f"| {r['scenario']} | BLOCKED: {r['blocked']} |||||||")
	        continue
	    kids = r["children"]
	    kid = f"{kids[0]['harness']} {kids[0]['version'] or ''}, {r['child_model'] or 'default model'}" if kids else "-"
	    tok = lambda u: str(u["total"]) if u else "?"
	    root_tok = tok(r["root"]["usage"]) if r["root"] else "?"
	    kid_tok = "+".join(tok(k["usage"]) for k in kids) or "-"
	    steer = "-"
	    if r["steer"]:
	        recs = [s["record"] for s in r["steer_records"]]
	        steer = ", ".join(recs) or "not queued"
	    wall = f"{r['wall_secs']}s" + (" (timed out)" if r["timed_out"] else "")
	    print(f"| {r['scenario']} | {yn(r['delegated'])} | {kid} | {yn(r['reported'])} | {r['verify']} | {yn(r['branch_landed'])} | {wall} | {root_tok}/{kid_tok} | {steer} |")
EOF
