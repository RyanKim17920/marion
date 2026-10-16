#!/bin/sh
# scripts/release-cooldown.sh against canned registry answers, at a fixed clock. Run by CI's lint
# job; prints each case and fails on the first one that disagrees.

set -eu

cd "$(dirname "$0")"

# 2026-10-16T12:00:00Z.
NOW=1792152000
export NOW
fails=0

expect() {
	want=$1 label=$2 published=$3
	got=0
	out=$(./release-cooldown.sh 7 "$published") || got=$?
	if [ "$got" -eq "$want" ]; then
		printf 'ok   %-28s exit %s: %s\n' "$label" "$got" "$out"
	else
		printf 'FAIL %-28s exit %s, wanted %s: %s\n' "$label" "$got" "$want" "$out"
		fails=$((fails + 1))
	fi
}

expect 2 "missing" ""
expect 2 "malformed" "yesterday"
expect 2 "a year fragment" "2026"
expect 2 "a date with no time" "2026-10-01"
expect 2 "an impossible date" "2026-13-45T99:99:99Z"
expect 2 "a local time, no Z" "2026-10-01T00:00:00"
expect 1 "young (one day)" "2026-10-15T12:00:00Z"
expect 1 "young (just inside)" "2026-10-09T12:00:01Z"
expect 0 "old (exactly seven days)" "2026-10-09T12:00:00Z"
expect 0 "old (thirty days)" "2026-09-16T12:00:00Z"
expect 0 "old, npm's milliseconds" "2026-09-16T12:00:00.123Z"
# Anything around a valid date makes the whole value unknown, and nothing in it is run.
# shellcheck disable=SC2016
expect 2 "a command substitution" '$(touch /tmp/x)2026-09-16T12:00:00Z'
expect 2 "two lines" "$(printf '2026-09-16T12:00:00Z\n2026-10-16T12:00:00Z')"

usage=0
./release-cooldown.sh x '' >/dev/null 2>&1 || usage=$?
if [ "$usage" -ne 64 ]; then
	echo "FAIL a non-numeric cooldown is a usage error"
	fails=$((fails + 1))
fi

if [ "$fails" -ne 0 ]; then
	echo "release-cooldown: $fails case(s) failed"
	exit 1
fi
echo "release-cooldown: all cases pass"
