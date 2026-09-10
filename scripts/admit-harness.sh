#!/bin/sh
# The harness admission ritual, scripted.
#
#   scripts/admit-harness.sh <harness> <version> [<harness> <version> ...]
#   e.g.  scripts/admit-harness.sh claude 2.1.263
#         scripts/admit-harness.sh opencode 1.18.30 goose 1.50.0
#
# A pinned harness auto-updated and the gate (`marion_testsupport::on_path`) went red: it names the
# installed version and the table's. The fix is never "widen the set"; it is to re-run everything
# that drives that harness against the new version and admit it WITH the evidence. This script does
# the mechanical part and stops short of the commit:
#
#   1. adds each <version> to its <harness>'s `accepted` list in
#      `marion_testsupport::PINNED_HARNESSES` (entry zero — the pin — is never touched);
#   2. runs `cargo test -p marion-testsupport --lib` (the table's own sanity tests), then every
#      integration suite under crates/marion-supervisor/tests that names one of the harnesses AND
#      gates on `on_path(`, plus the suites that walk every enabled native lane when a named
#      harness's lane is on — sequentially, each bounded by MARION_ADMIT_BOUND seconds (default
#      1800);
#   3. on all green, writes the dated observation comment above each `accepted` list, formats the
#      file, and prints the MILESTONES "Verified harness facts" paragraph(s) and the commit message.
#      On any red, restores the table exactly as it was and exits non-zero with the suite named
#      and every panic line of its output.
#
# Several pairs at once exist for one reason: `cross_product` drives every pinned harness in one
# binary, so when two of them drift on the same day neither can be admitted alone — the other's
# cells refuse at their own pin. One evidence run then covers both entries, and each entry's
# observation names the run it came from.
#
# It does not commit. Read the diff, paste the paragraph, then commit — with the probes under
# `spikes/` re-run beside it where the entry's evidence calls for them.
set -u

usage() {
    echo "usage: $0 <harness> <version> [<harness> <version> ...]" >&2
    echo "       (harnesses named in PINNED_HARNESSES; dotted versions)" >&2
    exit 2
}
[ $# -ge 2 ] && [ $(($# % 2)) -eq 0 ] || usage

here=$(cd "$(dirname "$0")/.." && pwd) || exit 1
table="$here/crates/marion-testsupport/src/lib.rs"
facades="$here/crates/marion-core/src/native_facade.rs"
tests_dir="$here/crates/marion-supervisor/tests"
bound=${MARION_ADMIT_BOUND:-1800}

# Validate every pair before touching anything.
pairs=""
while [ $# -ge 2 ]; do
    case $1 in *[!a-z]*|'') usage;; esac
    case $2 in *[!0-9.]*|''|*.|.*) usage;; esac
    grep -q "program: \"$1\"," "$table" || {
        echo "$1 is not a program in PINNED_HARNESSES ($table)" >&2
        exit 2
    }
    pairs="$pairs$1 $2
"
    shift 2
done

backup=$(mktemp) || exit 1
cp "$table" "$backup" || exit 1
restore() { cp "$backup" "$table"; rm -f "$backup"; }

# --- 1. widen each entry ------------------------------------------------------------------------
# Perl, because the `accepted` list may span lines (claude's does) and sed has no multi-line match
# worth reading. The entry is `program: "<h>",` followed — comments between — by `accepted: &[...]`.
widen() {
    ADMIT_HARNESS=$1 ADMIT_VERSION=$2 perl -0pi -e '
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
    ' "$table"
}
printf '%s' "$pairs" | while read -r h v; do
    widen "$h" "$v" || exit 1
    echo "admit-harness: $h $v added to PINNED_HARNESSES (not yet committed)"
done || { restore; exit 1; }
rustfmt --edition 2024 --config skip_children=true "$table" || { restore; exit 1; }

# --- 2. the suites that drive these harnesses ---------------------------------------------------
# Named by grep, not by hand: a suite that mentions a harness as a whole word and gates on
# `on_path(` drives a real one (cross_product gates on `n.program`, so the word match carries it).
# A suite that iterates `production_native_facades().enabled_native_commands()` drives every
# enabled native lane without ever naming one, so the word grep cannot see it. When a named
# harness's lane is on (`Lane::new(true, NativeLane::new("<h>", …))` in the facade table), add
# those too.
lane_on() {
    ADMIT_HARNESS=$1 perl -0ne 'exit !(/Lane::new\(\s*true,\s*NativeLane::new\("\Q$ENV{ADMIT_HARNESS}\E"/)' "$facades"
}
suites=""
lane_suites=""
for h in $(printf '%s' "$pairs" | awk '{print $1}'); do
    suites="$suites
$(grep -lw "$h" "$tests_dir"/*.rs | xargs grep -l 'on_path(' | xargs -n1 basename | sed 's/\.rs$//')"
    if lane_on "$h"; then
        lane_suites=$(grep -l 'enabled_native_commands()' "$tests_dir"/*.rs | xargs grep -l 'on_path(' | xargs -n1 basename | sed 's/\.rs$//')
    fi
done
suites=$(printf '%s\n%s\n' "$suites" "$lane_suites" | sed '/^$/d' | sort -u)
[ -n "$suites" ] || { echo "no suite under $tests_dir names these harnesses and gates on on_path(" >&2; restore; exit 1; }
echo "admit-harness: suites driving" $(printf '%s' "$pairs" | awk '{print $1}') ":" $suites

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
        echo "admit-harness: RED in $label; the table is restored. Every panic, then its tail:" >&2
        grep -E "panicked at|^test result:" "$log" >&2
        echo "---" >&2
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

# --- 3. the observation, beside each entry ------------------------------------------------------
stamp=$(date +%Y-%m-%d)
platform=$(uname -sr | tr -s ' ' ' ')
names=$(printf '%s' "$pairs" | awk '{printf "%s%s %s", (NR>1?" and ":""), $1, $2}')
annotate() {
    ADMIT_HARNESS=$1 ADMIT_NOTE="// $2: observed green on $platform, $stamp, via scripts/admit-harness.sh ($names in one run): $(printf '%s' "$counts" | tr -d '`')." perl -0pi -e '
        my $h = $ENV{ADMIT_HARNESS};
        s{(program: "\Q$h\E",.*?)(\n[ \t]*accepted: &\[)}{$1\n        $ENV{ADMIT_NOTE}$2}s or die;
    ' "$table"
}
printf '%s' "$pairs" | while read -r h v; do annotate "$h" "$v" || exit 1; done || { restore; exit 1; }
rustfmt --edition 2024 --config skip_children=true "$table" || { restore; exit 1; }
rm -f "$backup"

cat <<EOF

admit-harness: all green. Not committed. Review:  git diff -- crates/marion-testsupport/src/lib.rs

--- MILESTONES.md, "Verified harness facts", after the last admission paragraph -----------------
EOF
printf '%s' "$pairs" | while read -r h v; do
cat <<EOF

**$h $v, admitted $stamp via \`scripts/admit-harness.sh\`.** The binary first on
PATH moved to $v and the gate refused it; the suites that drive a real \`$h\` were
re-run against it (one run for $names), all green with only the table entries widened:
$counts. The probes under \`spikes/\` were **not** re-run by the script; those readings remain
the previous entry's unless re-measured beside this one.
EOF
done
cat <<EOF

--- commit message (one per harness when the paragraphs are split, or this for all) --------------

test: admit $names to the pinned harness set

The installed $names moved on this machine; every suite that drives
them was re-run against the new versions and passed with only the
accepted sets widened. Entry zero of each is unchanged: it is the
version the behaviours were measured on, not the version that ran today.
EOF
