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
#   s5  claude root -> pi child        fix the same off-by-one as s3
#   s6  opencode root -> codex child   add word_count(s), as s1, from an opencode root
#   s7  pi root -> codex child         add char_frequency(s), as s2, from a pi root
#   s39 a workflow, no root: plan on claude (haiku, read-only) -> race codex / opencode / pi ->
#       review on another model family -> at most two fixes -> land on its branch. Fix the
#       last_n_lines off-by-one. Runs only when named (`scripts/live-smoke.sh s39`): it starts
#       six or more agents, the suite's most expensive scenario. Its wall clock is
#       MARION_LIVE_SMOKE_S39_WALL_SECS (default 2400).
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
# Harnesses record the folders they run in: claude under `projects` in ~/.claude.json, codex as a
# `[projects."<repo>"] trust_level = "trusted"` table in ~/.codex/config.toml (measured in this
# suite's first run: one table per scenario repo), and claude keeps each folder's session history
# in ~/.claude/projects/<the path with every non-alphanumeric as '-'>. The suite backs both files up
# once (0600, in the run's scratch dir) and afterwards deletes exactly the entries under the run's
# scratch root, nothing else; the history dirs of the scratch paths move into the scratch dir. Results and redacted transcripts go to tests/fixtures/live-smoke-<date>/ (override with
# MARION_LIVE_SMOKE_OUT); read them against tests/fixtures/REVIEW.md before committing.
#
# MARION_LIVE_SMOKE_OPENCODE_MODEL names the model s3's prompt asks for, for an operator whose
# opencode default model is one their account cannot use (a subscription-only provider); s6's
# opencode root runs on it too. MARION_LIVE_SMOKE_PI_MODEL does the same for pi (s5's child, s7's
# root). Either unset leaves that harness on the operator's own default.

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
OPENCODE_MODEL=${MARION_LIVE_SMOKE_OPENCODE_MODEL:-}
PI_MODEL=${MARION_LIVE_SMOKE_PI_MODEL:-}

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

# ---- harness folder records: one backup, then only this run's own entries removed ----------------

CLAUDE_JSON="$HOME/.claude.json"
CODEX_CONFIG="${CODEX_HOME:-$HOME/.codex}/config.toml"
for f in "$CLAUDE_JSON" "$CODEX_CONFIG"; do
	[ -f "$f" ] && (umask 077 && cp -p "$f" "$RUN/$(basename "$f").bak")
done

prune_folder_records() {
	python3 - "$CLAUDE_JSON" "$CODEX_CONFIG" "$RUN" "${CLAUDE_CONFIG_DIR:-$HOME/.claude}/projects" <<-'EOF'
		import json, os, re, shutil, sys, tempfile
		claude_json, codex_config, scratch, claude_projects = sys.argv[1:5]
		prefixes = {scratch, os.path.realpath(scratch)}
		if scratch.startswith("/private/"):
		    prefixes.add(scratch[len("/private"):])
		ours = lambda k: any(k == p or k.startswith(p + "/") for p in prefixes)

		def replace(path, text):
		    fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path), prefix=".live-smoke.")
		    with os.fdopen(fd, "w") as f:
		        f.write(text)
		    os.chmod(tmp, os.stat(path).st_mode & 0o777)
		    os.replace(tmp, path)

		if os.path.isfile(claude_json):
		    with open(claude_json) as f:
		        doc = json.load(f)
		    projects = doc.get("projects") or {}
		    gone = [k for k in projects if ours(k)]
		    for k in gone:
		        del projects[k]
		    if gone:
		        replace(claude_json, json.dumps(doc, indent=2))
		        print(f"live-smoke: removed {len(gone)} ~/.claude.json project entries", file=sys.stderr)

		if os.path.isfile(codex_config):
		    kept, skip, gone = [], False, 0
		    for line in open(codex_config).read().split("\n"):
		        if line.startswith("["):
		            skip = line.startswith('[projects."') and ours(line[len('[projects."'):].split('"', 1)[0])
		            gone += skip
		        if not skip:
		            kept.append(line)
		    if gone:
		        replace(codex_config, "\n".join(kept))
		        print(f"live-smoke: removed {gone} codex config.toml project tables", file=sys.stderr)

		if os.path.isdir(claude_projects):
		    encoded = {re.sub(r"[^A-Za-z0-9]", "-", p) for p in prefixes}
		    moved = 0
		    for name in os.listdir(claude_projects):
		        if any(name == e or name.startswith(e + "-") for e in encoded):
		            os.makedirs(os.path.join(scratch, "claude-projects"), exist_ok=True)
		            shutil.move(os.path.join(claude_projects, name), os.path.join(scratch, "claude-projects", name))
		            moved += 1
		    if moved:
		        print(f"live-smoke: moved {moved} claude session-history dirs into the scratch dir", file=sys.stderr)
	EOF
}

cleanup() {
	prune_folder_records || echo "live-smoke: could not prune harness folder records; backups in $RUN" >&2
	# Worktrees a killed child left behind point into $RUN; drop the scratch repos with them, unless
	# MARION_LIVE_SMOKE_KEEP=1 asks to keep each scenario's repo and state dir for a diagnosis.
	[ "${MARION_LIVE_SMOKE_KEEP:-}" = 1 ] ||
		find "$RUN" -mindepth 1 -maxdepth 1 -type d -name 's[0-9]*' -exec rm -rf {} + 2>/dev/null || true
	echo "live-smoke: scratch kept at $RUN (config backups and moved claude session history)" >&2
}
trap cleanup EXIT

# ---- login status, read with each harness's own probe, never a login ----------------------------

logged_in() {
	# An agent type names its harness first: `pi-orchestrator` runs pi.
	case "${1%-orchestrator}" in
		claude) DISABLE_AUTOUPDATER=1 claude auth status --json 2>/dev/null | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("loggedIn") else 1)' ;;
		codex) codex -c check_for_update_on_startup=false login status >/dev/null 2>&1 ;;
		# opencode's status probe is a file (the README's profile table); no binary launch at all.
		opencode) [ -s "${XDG_DATA_HOME:-$HOME/.local/share}/opencode/auth.json" ] ;;
		# pi's too: its row's profile status is that <agent dir>/auth.json exists, never opened.
		pi) [ -e "${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}/auth.json" ] ;;
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

# scenario <id> <root harness> <root model|-> <child harness> <prompt> <check> [<steer text>] [wider]
#
# `wider` passes --allow-wider-children: the operator's opt-in for a child with more authority than
# its root, which a sandboxed root (codex) needs to start an unsandboxed child (claude on a real
# login). Stated per scenario and recorded in its result, never defaulted.
scenario() {
	local id=$1 root=$2 model=$3 child=$4 prompt=$5 check=$6 steer=${7:-} wider=${8:-}
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
	[ "$wider" = wider ] && args+=(--allow-wider-children)

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
		--timed-out "$timed_out" --steer "$steer" --wider-children "${wider:+yes}" --console "$dir/run.out" --steer-log "$dir/steer.out" \
		--out "$out" >/dev/null ||
		echo "live-smoke: $id: collecting results failed (see $dir)" >&2
	echo "live-smoke: $id done in $((end - start))s (marion run exit $rc)" >&2
	prune_folder_records
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
	'from textutil import char_frequency as f; assert f("aab") == {"a": 2, "b": 1}, f("aab"); assert f("") == {}' \
	"" wider

scenario s3 claude haiku opencode \
	"Delegate this to an opencode agent (agent_type \"opencode\")${OPENCODE_MODEL:+ with model \"$OPENCODE_MODEL\", spelled exactly so}: last_n_lines in textutil.py has an off-by-one bug. Fix it and add a regression test to test_textutil.py. The project's test command is \`$TEST_CMD\`." \
	'from textutil import last_n_lines as l; assert l("a\nb\nc", 2) == ["b", "c"], l("a\nb\nc", 2); assert l("a\nb\nc", 1) == ["c"]; assert l("a\nb\nc", 3) == ["a", "b", "c"]'

scenario s4 claude haiku codex \
	"Delegate this to a codex agent: add an average(nums) function to textutil.py that returns the arithmetic mean of a list of numbers, with unit tests in test_textutil.py. The project's test command is \`$TEST_CMD\`." \
	'from textutil import average as a; assert a([1, 2, 3]) == 2; assert a([]) == 0.0, "empty input was not handled"' \
	"Also handle empty input: average([]) must return 0.0 instead of raising. Add a test for it."

scenario s5 claude haiku pi \
	"Delegate this to a pi agent (agent_type \"pi\")${PI_MODEL:+ with model \"$PI_MODEL\", spelled exactly so}: last_n_lines in textutil.py has an off-by-one bug. Fix it and add a regression test to test_textutil.py. The project's test command is \`$TEST_CMD\`." \
	'from textutil import last_n_lines as l; assert l("a\nb\nc", 2) == ["b", "c"], l("a\nb\nc", 2); assert l("a\nb\nc", 1) == ["c"]; assert l("a\nb\nc", 3) == ["a", "b", "c"]'

# The roots that are not claude or codex: each delegates to codex, on the model its operator names.
scenario s6 opencode "${OPENCODE_MODEL:--}" codex \
	"Delegate this to a codex agent: add a word_count(s) function to textutil.py that returns the number of whitespace-separated words in s, with unit tests in test_textutil.py. The project's test command is \`$TEST_CMD\`." \
	'from textutil import word_count as w; assert w("a  b\nc") == 3, w("a  b\nc"); assert w("") == 0; assert w("   ") == 0'

scenario s7 pi-orchestrator "${PI_MODEL:--}" codex \
	"Delegate this to a codex agent: add a char_frequency(s) function to textutil.py that returns a dict mapping each character of s to how many times it occurs, with unit tests in test_textutil.py. The project's test command is \`$TEST_CMD\`." \
	'from textutil import char_frequency as f; assert f("aab") == {"a": 2, "b": 1}, f("aab"); assert f("") == {}'

# ---- a workflow scenario ----------------------------------------------------------------------------

# workflow_scenario <id> <workflow toml> <task> <check> <harness>...
#
# No root: `marion workflow run` drives the steps itself. The workflow is written into the fixture
# repository and trusted there by its bytes, and that trust entry is removed again afterwards, so
# the operator's trust store keeps nothing of the run. Judged on the run's own result.json, the
# journal's step intents, and the fixture's tests plus the check on the branch the run landed.
workflow_scenario() {
	local id=$1 toml=$2 task=$3 check=$4
	shift 4
	if [ "$ONLY" != "$id" ]; then
		return 0
	fi
	local dir="$RUN/$id" out="$OUT/$id"
	mkdir -p "$dir/state" "$out"
	for h in "$@"; do
		if [ -z "$CANNED" ] && ! logged_in "$h"; then
			echo "live-smoke: $id BLOCKED: $h is not logged in (log in with its own CLI, then rerun $id)" >&2
			printf '{"scenario":"%s","blocked":"%s not logged in"}\n' "$id" "$h" >"$out/result.json"
			return 0
		fi
	done
	make_fixture "$dir/repo"
	local file="$dir/repo/.marion/workflows/$id.toml"
	mkdir -p "$(dirname "$file")"
	printf '%s\n' "$toml" >"$file"
	cp "$file" "$out/workflow.toml"
	(cd "$dir/repo" && "$BIN" trust allow "$file") >"$dir/trust.out" 2>&1 || {
		echo "live-smoke: $id: marion trust allow refused the workflow (see $dir/trust.out)" >&2
		return 0
	}

	local args=(workflow run "$id" --task "$task" --repo "$dir/repo" --state-dir "$dir/state")
	[ -n "$CANNED" ] && args+=(--canned)
	echo "live-smoke: $id: workflow run (wall ${WALL_SECS}s)" >&2
	local start end rc=0 timed_out=no
	start=$(date +%s)
	(cd "$dir/repo" && exec timeout -k 15 "$WALL_SECS" "$BIN" "${args[@]}") >"$dir/run.out" 2>&1 || rc=$?
	end=$(date +%s)
	[ "$rc" -eq 124 ] || [ "$rc" -eq 137 ] && timed_out=yes
	if [ "$timed_out" = yes ]; then
		# The supervisor carries a run on after its client is killed: stop the run first, so it
		# launches nothing more, then whatever still runs.
		local wf
		wf=$(sed -n 's/^marion: workflow .* is running as \([^ ]*\) .*/\1/p' "$dir/run.out" | head -1)
		[ -n "$wf" ] && { (cd "$dir/repo" && "$BIN" workflow cancel "$wf" --force --state-dir "$dir/state") >>"$dir/run.out" 2>&1 || true; }
		stop_leftovers "$dir"
	fi
	(cd "$dir/repo" && "$BIN" trust deny "$file") >>"$dir/trust.out" 2>&1 ||
		echo "live-smoke: $id: could not revoke the scratch workflow's trust entry; \`marion trust list\` shows it" >&2

	python3 - "$dir" "$out" "$id" "$TEST_CMD" "$check" $((end - start)) "$timed_out" "$rc" "$COLLECT" "$RUN" <<-'EOF' ||
		import importlib.util, json, pathlib, subprocess, sys
		dir, out, scenario, test_cmd, check, wall, timed_out, rc, collect, scratch = sys.argv[1:11]
		dir, out = pathlib.Path(dir), pathlib.Path(out)
		# The suite's own collector: its redaction, node walk and transcript shape, one copy.
		spec = importlib.util.spec_from_file_location("collect", collect)
		c = importlib.util.module_from_spec(spec)
		spec.loader.exec_module(c)
		red = c.Redactor(scratch)
		for f in ["run.out", "trust.out"]:
		    if (dir / f).exists():
		        (out / f).write_text(red.text((dir / f).read_text(errors="replace")))
		pdirs = [p for p in (dir / "state").iterdir() if (p / "journal.jsonl").exists()]
		walked = c.walk_nodes(pdirs[0])[0] if pdirs else []
		by_agent = {path.parent.parent.name: k for path, k in c.contracts(pdirs[0])} if pdirs else {}
		for n in walked:
		    events = c.read_jsonl(pdirs[0] / "agents" / n["agent_id"] / "events.jsonl")
		    n["cost_usd"] = c.node_cost_usd(events)
		    name = f"node{walked.index(n) + 1}-{n['harness']}"
		    (out / f"{name}.transcript.json").write_text(
		        json.dumps(c.transcript(events, red), indent=1, ensure_ascii=False) + "\n")
		    if n["agent_id"] in by_agent:
		        (out / f"contract-{name}.json").write_text(
		            json.dumps(red.value(by_agent[n["agent_id"]], cut=False), indent=1, ensure_ascii=False) + "\n")
		results = sorted((dir / "state").glob("*/workflows/*/result.json"))
		r = {"scenario": scenario, "workflow": True, "wall_secs": int(wall),
		     "timed_out": timed_out == "yes", "exit": int(rc)}
		if not results:
		    r["outcome"] = "never closed"
		    (out / "result.json").write_text(json.dumps(r, indent=2) + "\n")
		    sys.exit(0)
		res = json.loads(results[-1].read_text())
		r["outcome"] = res["outcome"] if isinstance(res["outcome"], str) else next(iter(res["outcome"]))
		r["steps"] = [{"id": s["id"], "kind": s["kind"], "verdict": s.get("verdict"),
		               "nodes": len(s.get("nodes", [])), "note": s.get("note")} for s in res["steps"]]
		intents = {}
		for j in (dir / "state").glob("*/journal.jsonl"):
		    for line in j.read_text(errors="replace").splitlines():
		        try:
		            k = json.loads(line).get("kind")
		        except json.JSONDecodeError:
		            continue
		        if isinstance(k, dict) and "SpawnIntent" in k and k["SpawnIntent"].get("workflow"):
		            i = k["SpawnIntent"]
		            intents[i["agent_id"]] = i
		by_step = lambda n: [i for i in intents.values() if i["workflow"].get("step", 0) == n]
		harness = lambda i: i.get("harness") if isinstance(i.get("harness"), str) else json.dumps(i.get("harness"))
		plan, race, review = by_step(0), by_step(1), by_step(2)
		r["plan"] = [harness(i) for i in plan]
		r["race_seats"] = [harness(i) for i in race]
		winner = next((s.get("work") for s in res["steps"] if s["kind"] == "race"), None)
		r["winner"] = harness(intents[winner["agent_id"]]) if winner and winner["agent_id"] in intents else None
		r["reviewers"] = [harness(i) for i in review if i["workflow"].get("part", 0) == 0]
		r["fixes"] = sum(1 for i in review if i["workflow"].get("part", 0) == 1)
		r["reviewer_family_differs"] = bool(r["winner"]) and all(h != r["winner"] for h in r["reviewers"])
		land = next((s for s in res["steps"] if s["kind"] == "land"), {})
		r["landed"] = land.get("note")
		branch = None
		if land.get("note") and "changes on branch " in land["note"]:
		    branch = land["note"].split("changes on branch ", 1)[1].split(" ", 1)[0]
		r["branch"] = branch
		r["verify"] = "not landed"
		if branch:
		    tree = dir / "landed"
		    subprocess.run(["git", "-C", str(dir / "repo"), "worktree", "add", "-q", "--detach", str(tree), branch], check=False)
		    tests = subprocess.run(test_cmd.split(), cwd=tree, capture_output=True, text=True)
		    probe = subprocess.run(["python3", "-c", check], cwd=tree, capture_output=True, text=True)
		    r["verify"] = "pass" if tests.returncode == 0 and probe.returncode == 0 else "fail"
		    r["verify_output"] = (tests.stdout + tests.stderr + probe.stderr)[-2000:]
		    subprocess.run(["git", "-C", str(dir / "repo"), "worktree", "remove", "--force", str(tree)], check=False)
		r["nodes"] = [{"harness": n["harness"], "agent_type": n["agent_type"], "exit": n["exit"],
		               "tokens": sum(n["usage"].get(k) or 0 for k in ("input", "output", "cache_read", "cache_write")) if n["usage"] else None,
		               "cost_usd": n["cost_usd"]} for n in walked]
		r["tokens"] = sum(n["tokens"] or 0 for n in r["nodes"])
		r["unrecorded"] = sum(1 for n in r["nodes"] if n["tokens"] is None)
		(out / "result.json").write_text(json.dumps(red.value(r, cut=False), indent=2) + "\n")
	EOF
		echo "live-smoke: $id: collecting results failed (see $dir)" >&2
	echo "live-smoke: $id done in $((end - start))s (marion workflow run exit $rc)" >&2
	prune_folder_records
}

S39_OPENCODE="opencode${OPENCODE_MODEL:+:$OPENCODE_MODEL}"
# Six agents or more, one after another: its own wall clock, the run's and the scenario's both.
S39_WALL_SECS=${MARION_LIVE_SMOKE_S39_WALL_SECS:-2400}
WALL_SECS=$S39_WALL_SECS workflow_scenario s39 "schema = 1
name = \"s39\"
inputs = [\"task\"]
budget = { wall = \"$((S39_WALL_SECS / 60))m\" }

[[step]]
id = \"plan\"
kind = \"agent\"
on = \"claude:haiku\"
read_only = true
prompt = \"Read the code and write a short plan for this task. Do not change any file.\\n\\nTask: {input.task}\"

[[step]]
id = \"build\"
kind = \"race\"
on = [\"codex\", \"$S39_OPENCODE\", \"pi\"]
prompt = \"{input.task}\\n\\nA plan another agent wrote:\\n{plan.report}\"
verify = [\"$TEST_CMD\"]

[[step]]
id = \"gate\"
kind = \"review\"
of = \"build\"
max_rounds = 3

[[step]]
id = \"land\"
kind = \"land\"
of = \"gate\"
mode = \"branch\"" \
	"last_n_lines in textutil.py has an off-by-one bug. Fix it and add a regression test to test_textutil.py. The project's test command is \`$TEST_CMD\`." \
	'from textutil import last_n_lines as l; assert l("a\nb\nc", 2) == ["b", "c"], l("a\nb\nc", 2); assert l("a\nb\nc", 1) == ["c"]' \
	claude codex opencode pi

# ---- the table --------------------------------------------------------------------------------------

python3 - "$OUT" <<-'EOF' | tee "$OUT/results.md"
	import json, pathlib, sys
	out = pathlib.Path(sys.argv[1])
	print("| scenario | delegated | expected child: model, status | reported | verify on branch | landed | wall | tokens root / children | every child (harness@depth: exit) | steer |")
	print("|---|---|---|---|---|---|---|---|---|---|")
	yn = lambda b: "y" if b else "n"
	for p in sorted(out.glob("s*/result.json")):
	    r = json.loads(p.read_text())
	    if "blocked" in r:
	        print(f"| {r['scenario']} | BLOCKED: {r['blocked']} ||||||||")
	        continue
	    if r.get("workflow"):
	        steps = ", ".join(f"{s['id']}: {s['verdict']}" for s in r.get("steps", []))
	        seats = "/".join(r.get("race_seats", []))
	        review = f"reviewers {'/'.join(r.get('reviewers', []))}, other family {yn(r.get('reviewer_family_differs'))}, fixes {r.get('fixes')}"
	        wall = f"{r['wall_secs']}s" + (" (timed out)" if r["timed_out"] else "")
	        tokens = f"- / {r.get('tokens')}" + (f" (+{r['unrecorded']} unrecorded)" if r.get("unrecorded") else "")
	        every = ", ".join(f"{n['harness']}: {n['exit']}" for n in r.get("nodes", [])) or "-"
	        print(f"| {r['scenario']} | workflow {r['outcome']}: {steps} | race {seats}, won {r.get('winner')} | {review} | {r.get('verify')} | {yn(r.get('branch'))} | {wall} | {tokens} | {every} | - |")
	        continue
	    kids = r["children"]
	    expected = f"{r['child_harness_expected']}" + ("" if r["child_harness_matches"] else " (never ran)")
	    expected += f": {r['child_model'] or 'default model'}, {r['status']}"
	    # A node killed at its timeout records no usage: marion has no claim, which is not zero.
	    tok = lambda u: str(u["total"]) if u else "none recorded"
	    root_tok = tok(r["root"]["usage"]) if r["root"] else "?"
	    kid_tok = sum(k["usage"]["total"] for k in kids if k["usage"])
	    unknown = sum(1 for k in kids if not k["usage"])
	    kid_tok = f"{kid_tok}" + (f" (+{unknown} unrecorded)" if unknown else "")
	    every = ", ".join(f"{k['harness']}@{k['depth']}: {k['exit']}" for k in kids) or "-"
	    steer = "-"
	    if r["steer"]:
	        recs = [s["record"] + (f" via {s['detail'].get('via')}" if s["detail"].get("via") else "") for s in r["steer_records"]]
	        steer = ", ".join(recs) or "not queued"
	    wall = f"{r['wall_secs']}s" + (" (timed out)" if r["timed_out"] else "")
	    wider = " (--allow-wider-children)" if r.get("wider_children") else ""
	    print(f"| {r['scenario']}{wider} | {yn(r['delegated'])} | {expected} | {yn(r['reported'])} | {r['verify']} | {yn(r['branch_landed'])} | {wall} | {root_tok} / {kid_tok} | {every} | {steer} |")
EOF
