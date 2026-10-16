#!/bin/sh
# List harness releases as verified in their rows.
#
#   scripts/admit-harness.sh <harness> <version> [<harness> <version> ...]
#   e.g.  scripts/admit-harness.sh claude 2.1.301 opencode 1.19.3
#
# Adds each <version> to the `verified` list of the row whose spec says `program: Some("<harness>")`
# (crates/marion-harness/src/), then formats those files. It builds and runs nothing, and does not
# commit.
#
# The evidence a release needs is the conformance battery's: `scripts/conformance.sh --harness
# <row>` records whether it admits that exact binary, and the test gate (marion_testsupport::on_path)
# already drives a release its record admits. Listing it shares that verdict with every machine;
# the nightly canary (.github/workflows/canary.yml) runs this for every release the battery
# admitted. List one by hand only after reading why the battery did not: a P-approval failure that
# names a moved ask key, for one, is a wire change to read before the release becomes the bar.
set -u

usage() {
    echo "usage: $0 <harness> <version> [<harness> <version> ...]" >&2
    exit 2
}
[ $# -ge 2 ] && [ $(($# % 2)) -eq 0 ] || usage

rows_dir="$(cd "$(dirname "$0")/.." && pwd)/crates/marion-harness/src" || exit 1
here=${rows_dir%/crates/marion-harness/src}
row_of() { grep -l "program: Some(\"$1\")," "$rows_dir"/*.rs | head -n 1; }

# Validate every pair before touching anything.
pairs=""
while [ $# -ge 2 ]; do
    case $1 in *[!a-z]*|'') usage;; esac
    case $2 in *[!0-9.]*|''|*.|.*) usage;; esac
    [ -n "$(row_of "$1")" ] || { echo "no harness row runs $1 (in $rows_dir)" >&2; exit 2; }
    # One version per program: a run lists the one release it installed.
    case " $(printf '%s' "$pairs" | awk '{print $1}' | tr '\n' ' ') " in
        *" $1 "*) echo "$1 is named twice" >&2; exit 2;;
    esac
    pairs="$pairs$1 $2
"
    shift 2
done

# Perl, because the `verified` list may span lines (claude's does). The row is
# `program: Some("<h>"),` followed — comments between — by `verified: &[...]`. A version that is not
# strictly newer than every listed one is refused: listing an old release is a downgrade, never an
# admission, and nothing ever needs one.
printf '%s' "$pairs" | while read -r h v; do
    row=$(row_of "$h")
    ADMIT_HARNESS=$h ADMIT_VERSION=$v perl -0pi -e '
        my ($h, $v) = ($ENV{ADMIT_HARNESS}, $ENV{ADMIT_VERSION});
        s{(program: Some\("\Q$h\E"\),.*?verified: &\[)([^\]]*)\]}{
            my ($head, $list) = ($1, $2);
            my $cmp = sub {
                my @a = split /\./, $_[0]; my @b = split /\./, $_[1];
                while (@a || @b) { my $d = (shift(@a) // 0) <=> (shift(@b) // 0); return $d if $d }
                0
            };
            for my $old ($list =~ /"([0-9.]+)"/g) {
                $cmp->($v, $old) > 0 or die "$h $v is not newer than the listed $old\n";
            }
            {
                $list =~ s/\s+$//;
                $list .= "," unless $list =~ /,$/ || $list eq "";
                $head . $list . " \"$v\"" . "]"
            }
        }se or die "no verified list found for $h\n";
    ' "$row" || exit 1
    rustfmt --edition 2024 --config skip_children=true "$row" || exit 1
    # The exact file, relative to the repository, for a caller that commits only what this edited.
    echo "admit-harness: $h $v listed in ${row#"$here/"}"
done
