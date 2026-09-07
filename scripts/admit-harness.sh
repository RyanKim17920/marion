#!/bin/sh
# The harness admission ritual, scripted.
#
#   scripts/admit-harness.sh <harness> <version>      e.g.  scripts/admit-harness.sh claude 2.1.263
#
# A pinned harness auto-updated and the gate (`marion_testsupport::on_path`) went red: it names the
# installed version and the table's. The fix is never "widen the set"; it is to re-run everything
# that drives that harness against the new version and admit it WITH the evidence. This script does
# the mechanical part and stops short of the commit:
#
#   1. adds <version> to <harness>'s `accepted` list in `marion_testsupport::PINNED_HARNESSES`
#      (entry zero — the pin — is never touched);
#   2. runs `cargo test -p marion-testsupport --lib` (the table's own sanity tests), then every
#      integration suite under crates/marion-supervisor/tests that names <harness> AND gates on
#      `on_path(` — sequentially, each bounded by MARION_ADMIT_BOUND seconds (default 1800);
#   3. on all green, writes the dated observation comment above the `accepted` list, formats the
#      file, and prints the MILESTONES "Verified harness facts" paragraph and the commit message.
#      On any red, restores the table exactly as it was and exits non-zero with the suite named.
#
# It does not commit. Read the diff, paste the paragraph, then commit — with the probes under
# `spikes/` re-run beside it where the entry's evidence calls for them.
set -u

usage() {
    echo "usage: $0 <harness> <version>    (a harness named in PINNED_HARNESSES; a dotted version)" >&2
    exit 2
}
[ $# -eq 2 ] || usage
harness=$1
version=$2
case $harness in *[!a-z]*|'') usage;; esac
case $version in *[!0-9.]*|''|*.|.*) usage;; esac

here=$(cd "$(dirname "$0")/.." && pwd) || exit 1
table="$here/crates/marion-testsupport/src/lib.rs"
tests_dir="$here/crates/marion-supervisor/tests"
bound=${MARION_ADMIT_BOUND:-1800}

grep -q "program: \"$harness\"," "$table" || {
    echo "$harness is not a program in PINNED_HARNESSES ($table)" >&2
    exit 2
}

backup=$(mktemp) || exit 1
cp "$table" "$backup" || exit 1
restore() { cp "$backup" "$table"; rm -f "$backup"; }

# --- 1. widen the entry -------------------------------------------------------------------------
# Perl, because the `accepted` list may span lines (claude's does) and sed has no multi-line match
# worth reading. The entry is `program: "<h>",` followed — comments between — by `accepted: &[...]`.
export ADMIT_HARNESS=$harness ADMIT_VERSION=$version
perl -0pi -e '
    my ($h, $v) = ($ENV{ADMIT_HARNESS}, $ENV{ADMIT_VERSION});
    s{(program: "\Q$h\E",.*?accepted: &\[)([^\]]*)\]}{
        my ($head, $list) = ($1, $2);
        if ($list =~ /"\Q$v\E"/) { $head . $list . "]" }
        else {
            $list =~ s/\s+$//;
            $list .= "," unless $list =~ /,$/ || $list eq "";
            $head . $list . " \"$v\"" . "]"
        }
    }se or die "no accepted list found for $h\n";
' "$table" || { restore; exit 1; }
rustfmt --edition 2024 --config skip_children=true "$table" || { restore; exit 1; }
echo "admit-harness: $harness $version added to PINNED_HARNESSES (not yet committed)"

# --- 2. the suites that drive this harness ------------------------------------------------------
# Named by grep, not by hand: a suite that mentions the harness as a whole word and gates on
# `on_path(` drives a real one (cross_product gates on `n.program`, so the word match carries it).
suites=$(grep -lw "$harness" "$tests_dir"/*.rs | xargs grep -l 'on_path(' | xargs -n1 basename | sed 's/\.rs$//' | sort)
[ -n "$suites" ] || { echo "no suite under $tests_dir names $harness and gates on on_path(" >&2; restore; exit 1; }

# One command, bounded: the child is put in its own process group (perl's setpgrp) so the bound can
# kill the whole tree — cargo, the test binary, the harnesses it spawned — and not the script.
bounded() {
    perl -e 'setpgrp(0, 0); exec @ARGV or die "exec: $!\n"' "$@" &
    pid=$!
    ( sleep "$bound"; kill -TERM -- "-$pid" 2>/dev/null; sleep 5; kill -KILL -- "-$pid" 2>/dev/null ) &
    watchdog=$!
    wait "$pid"
    rc=$?
    kill "$watchdog" 2>/dev/null
    wait "$watchdog" 2>/dev/null
    return $rc
}

log=$(mktemp) || { restore; exit 1; }
counts=""
run_suite() {
    label=$1; shift
    echo "admit-harness: running $label (bound ${bound}s)"
    if ! bounded "$@" >"$log" 2>&1; then
        echo "admit-harness: RED in $label; the table is restored. Its output:" >&2
        tail -n 60 "$log" >&2
        restore; rm -f "$log"; exit 1
    fi
    passed=$(grep -o 'test result: ok\. [0-9]* passed' "$log" | awk '{s+=$4} END {print s+0}')
    counts="$counts${counts:+, }\`$label\` ($passed)"
}

cd "$here" || exit 1
run_suite marion-testsupport cargo test -p marion-testsupport --lib
for s in $suites; do
    run_suite "$s" cargo test -p marion-supervisor --test "$s"
done
rm -f "$log"

# --- 3. the observation, beside the entry -------------------------------------------------------
stamp=$(date +%Y-%m-%d)
platform=$(uname -sr | tr -s ' ' ' ')
export ADMIT_NOTE="// $version: observed green on $platform, $stamp, via scripts/admit-harness.sh: $(printf '%s' "$counts" | tr -d '`')."
perl -0pi -e '
    my $h = $ENV{ADMIT_HARNESS};
    s{(program: "\Q$h\E",.*?)(\n[ \t]*accepted: &\[)}{$1\n        $ENV{ADMIT_NOTE}$2}s or die;
' "$table" || { restore; exit 1; }
rustfmt --edition 2024 --config skip_children=true "$table" || { restore; exit 1; }
rm -f "$backup"

cat <<EOF

admit-harness: all green. Not committed. Review:  git diff -- crates/marion-testsupport/src/lib.rs

--- MILESTONES.md, "Verified harness facts", after the last admission paragraph -----------------

**$harness $version, admitted $stamp via \`scripts/admit-harness.sh\`.** The binary first on
PATH moved to $version and the gate refused it; the suites that drive a real \`$harness\` were
re-run against it, all green with only the table entry widened: $counts. The probes under
\`spikes/\` were **not** re-run by the script; those readings remain the previous entry's unless
re-measured beside this one.

--- commit message ------------------------------------------------------------------------------

test: admit $harness $version to the pin table

The installed $harness moved to $version; every suite that drives a real
$harness was re-run against it and passed with only the accepted set
widened. Entry zero is unchanged: it is the version the behaviours were
measured on, not the version that ran today.
EOF
