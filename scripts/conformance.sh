#!/bin/sh
# Run the harness conformance battery (crates/marion-supervisor/tests/conformance).
#
#   scripts/conformance.sh --harness opencode [--harness acp:opencode ...] [--out DIR] [--baseline FILE]
#   scripts/conformance.sh --all [--out DIR]
#
# Every probe is defined once and reads the row: a harness gets the whole battery by having a row.
# $0: marion's canned provider on loopback, scratch homes, no login. Rows whose binary is not on
# PATH are skipped by name. Writes <out>/<row>-<version>/ transcripts and <out>/matrix.{json,md}
# (default out: tests/fixtures/conformance/). With --baseline, a cell that was PASS in that matrix
# and is not now fails the run. MARION_CONFORMANCE_PROBES=P-errors,P-tui runs only those probes;
# for a row whose version is unchanged, the matrix and its directory keep every other probe's last
# cell and transcript, so a single probe can be refreshed in the committed fixtures.
#
# Callers: scripts/admit-harness.sh runs it for the admitted programs against the committed matrix.
# TODO(release-canary): the nightly canary (.github/workflows/canary.yml on branch release-canary,
# not yet landed) should run, after its harness_drift step and on the newest installed harnesses,
#   scripts/conformance.sh --all --out "$RUNNER_TEMP/conformance" \
#       --baseline tests/fixtures/conformance/matrix.json
# and attach "$RUNNER_TEMP/conformance/matrix.md" to the drift PR it opens, so a behaviour change
# the suites do not assert (MCP readiness, provider faults, mid-turn writes) is seen the night it
# ships rather than at the next admission.
set -u

here=$(cd "$(dirname "$0")/.." && pwd) || exit 1
rows=""
out=""
baseline=""
usage() {
    echo "usage: $0 (--all | --harness <row> [--harness <row> ...]) [--out DIR] [--baseline FILE]" >&2
    exit 2
}
while [ $# -gt 0 ]; do
    case $1 in
        --all) rows="all"; shift ;;
        --harness) [ $# -ge 2 ] || usage; rows="${rows:+$rows,}$2"; shift 2 ;;
        --out) [ $# -ge 2 ] || usage; out=$2; shift 2 ;;
        --baseline) [ $# -ge 2 ] || usage; baseline=$2; shift 2 ;;
        *) usage ;;
    esac
done
[ -n "$rows" ] || usage

cd "$here" || exit 1
out=${out:-$here/tests/fixtures/conformance}
MARION_CONFORMANCE=$rows \
MARION_CONFORMANCE_OUT=$out \
MARION_CONFORMANCE_BASELINE=$baseline \
COPILOT_AUTO_UPDATE=false \
    cargo test -p marion-supervisor --test conformance -- battery --nocapture
status=$?
# A capture on the operator's own profile prints what that profile holds; refuse to leave any of
# it in the transcripts (scripts/fixture-privacy.py says what it looks for).
"$here/scripts/fixture-privacy.py" "$out" || status=1
exit $status
