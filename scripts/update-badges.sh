#!/usr/bin/env bash
# Regenerates README.md's badge numbers from ground truth -- run this instead
# of hand-editing them, so they can't drift from reality the way manually
# maintained ones always do (same convention as PCBBench's).
#
#   tests       passing unit tests across the workspace (`cargo test`)
#   families    curated circuit families `lob spec` decides (family::FAMILIES)
#   datasheets  datasheets pinned by URL + SHA-256 for cited facts
#
# Fails loud (set -e + pipefail) if any test fails or any count comes out
# zero: a badge update is refused, not silently computed from a broken run.
set -euo pipefail
cd "$(dirname "$0")/.."

output="$(cargo test --workspace 2>&1)"
tests="$(grep -oE '^test result: ok\. [0-9]+ passed' <<<"$output" \
    | grep -oE '[0-9]+' \
    | awk '{s+=$1} END {print s+0}')"
families="$(grep -E '^pub const FAMILIES' crates/core/src/family.rs \
    | grep -oE '"[^"]+"' | wc -l | tr -d ' ')"
datasheets="$(grep -rhE ': Datasheet = Datasheet \{' crates/core/src | wc -l | tr -d ' ')"

for pair in "tests:$tests" "families:$families" "datasheets:$datasheets"; do
    if [ "${pair#*:}" -eq 0 ]; then
        echo "update-badges: found 0 ${pair%%:*} -- refusing to write a bogus badge" >&2
        [ "${pair%%:*}" = tests ] && echo "$output" >&2
        exit 1
    fi
done

sed -E \
    -e "s#(badge/tests-)[0-9]+(%20passing)#\\1${tests}\\2#" \
    -e "s#(badge/curated%20families-)[0-9]+#\\1${families}#" \
    -e "s#(badge/pinned%20datasheets-)[0-9]+#\\1${datasheets}#" \
    README.md > README.md.tmp
mv README.md.tmp README.md

echo "tests: ${tests}, families: ${families}, datasheets: ${datasheets}"
