#!/bin/sh
# Is a harness release old enough to list? The canary's `admit` job asks this of every release it
# would add to a row's `verified` list: most hijacked releases are caught and pulled within days,
# so a release is listed only once its publish date is COOLDOWN_DAYS old.
#
# Usage: scripts/release-cooldown.sh <cooldown-days> <published>
#
# <published> is the registry's own timestamp (npm's `time.<version>`, a GitHub release's
# `published_at`), ISO 8601 UTC with optional fractional seconds. It comes from outside this
# repository, so it is read only through that one shape: anything else (empty, a fragment such as
# `2026`, prose) is unknown, and an unknown date never lists.
#
# Prints one line and exits 0 when the release is old enough, 1 when it is inside the cooldown,
# 2 when its date is unknown. NOW (seconds since the epoch) replaces the clock, for the test.

set -eu

days=${1:?usage: release-cooldown.sh <cooldown-days> <published>}
published=${2-}

case "$days" in
'' | *[!0-9]*)
	echo "release-cooldown: cooldown days must be a whole number, not '$days'" >&2
	exit 64
	;;
esac

# The whole value must be that shape, so nothing else reaches `date` or the log. Stripping
# foreign characters instead would turn `junk2026-09-16T12:00:00Z` into a date, and `grep` alone
# would pass a value with one good line among others.
nl='
'
case "$published" in *"$nl"*) published="" ;; esac
if ! printf '%s\n' "$published" |
	grep -Eq '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]+)?Z$'; then
	echo "unknown publish date"
	exit 2
fi
whole=${published%%.*}
whole=${whole%Z}

# GNU date (the runner) and BSD date (a developer's Mac) spell a fixed-format parse differently.
when=$(date -u -d "${whole}Z" +%s 2>/dev/null ||
	date -j -u -f '%Y-%m-%dT%H:%M:%S' "$whole" +%s 2>/dev/null ||
	true)
if [ -z "$when" ]; then
	echo "unknown publish date"
	exit 2
fi

now=${NOW:-$(date -u +%s)}
if [ "$when" -gt $((now - days * 86400)) ]; then
	echo "published ${whole}Z, inside the ${days}-day cooldown"
	exit 1
fi
echo "published ${whole}Z, past the ${days}-day cooldown"
