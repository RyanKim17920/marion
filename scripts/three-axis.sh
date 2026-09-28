#!/bin/sh
# marion's three-axis checks — harness generality, efficiency, security — with readable output.
#
# The checks are an ordinary integration test (crates/marion-testsupport/tests/three_axis/), so
# `cargo test` and CI run them too; this script only runs that one target and prints each axis's
# verdict and, on failure, the report: every new finding with its file:line, the allowlist key to
# use if it truly must stay, and what to do instead. Allowlists live in checks/<axis>.allow and
# only shrink: an entry nothing matches any more fails the check until its line is deleted.
#
# Usage: scripts/three-axis.sh [generality|efficiency|security]   (default: all three)

set -eu

cd "$(git rev-parse --show-toplevel)"

filter=${1:-}
case "$filter" in
	"" | generality | efficiency | security) ;;
	*)
		printf 'usage: %s [generality|efficiency|security]\n' "$0" >&2
		exit 2
		;;
esac

printf 'three-axis: cargo test -p marion-testsupport --test three_axis %s\n' "$filter" >&2

status=0
output=$(cargo test -p marion-testsupport --test three_axis -- --nocapture --test-threads=1 $filter 2>&1) || status=$?

# The per-axis verdict lines and the failure reports, without cargo's build chatter. `--nocapture`
# can put a verdict on the same line as libtest's `test x ... `, so match it anywhere in the line.
printf '%s\n' "$output" | awk '
	/three-axis [A-Z][a-z]*: / { sub(/.*three-axis /, "three-axis "); print; next }
	/check failed\./ { report = 1 }
	/panicked at|^note: run with/ { report = 0 }
	report { print }
'

if [ "$status" -ne 0 ]; then
	# A failure the report above does not explain (a compile error, a detector test) shows whole.
	if ! printf '%s\n' "$output" | grep -q 'check failed\.'; then
		printf '%s\n' "$output" >&2
	fi
	printf '\nthree-axis: FAILED\n' >&2
	exit "$status"
fi

printf '%s\n' "$output" | grep '^test result:' | tail -1

# Security, part two: no committed capture may carry the operator's home path, email, or the
# skills, plugins and MCP servers installed on the machine running this check.
if [ -z "$filter" ] || [ "$filter" = security ]; then
	if ! scripts/fixture-privacy.py; then
		printf '\nthree-axis: FAILED (fixture privacy)\n' >&2
		exit 1
	fi
fi
printf 'three-axis: ok\n' >&2
