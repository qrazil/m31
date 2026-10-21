#!/usr/bin/env bash
# Heap watch's cost (docs/heap-watch-design.md, "Cost"): each program built
# once and run with heap watch on, as every program is by default, and with
# M31_HEAPWATCH=0 -- the same binary, so the difference is the counting, the
# ledger and the epochs. What stays with it off is the store hook's compare
# and the per-site records; comparing against a build of the commit before
# heap watch measures those too, which is what the design doc's table does.
#
# Interleaved, best of RUNS: the noise on this machine is other builds, and
# noise only ever adds.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
. ./config.sh
. ./runtime/arch.sh

LANGC=${LANGC:-./target/debug/$LANG_BIN}
CC=${CC:-gcc}
RUNS=${RUNS:-7}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
export M31_HEAPWATCH_DIR="$WORK/ledgers"

build() {
    "$LANGC" -emit-c "$1" --out "$WORK/$2.c" || return 1
    "$CC" -O2 -ffp-contract=off -I runtime "$WORK/$2.c" \
        runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" \
        -lpthread -o "$WORK/$2.bin" || return 1
}

seconds() {
    { /usr/bin/time -f '%e' env "$@" >/dev/null; } 2>&1 | tail -1
}

printf '%-14s %8s %8s %7s\n' benchmark off on ratio
for src in bench/trees/trees.m31 bench/heapwatch/allocs.m31 bench/heapwatch/stores.m31 \
           bench/valenum/result-boxed.m31 bench/valenum/option-boxed.m31; do
    name=$(basename "$src" ".$LANG_EXT")
    build "$src" "$name" || { echo "$name: build failed"; continue; }
    off="" on=""
    for _ in $(seq "$RUNS"); do
        t=$(seconds M31_HEAPWATCH=0 "$WORK/$name.bin")
        { [ -z "$off" ] || awk -v a="$t" -v b="$off" 'BEGIN{exit !(a<b)}'; } && off=$t
        t=$(seconds M31_HEAPWATCH=1 "$WORK/$name.bin")
        { [ -z "$on" ] || awk -v a="$t" -v b="$on" 'BEGIN{exit !(a<b)}'; } && on=$t
    done
    r=$(awk -v a="$off" -v b="$on" 'BEGIN{ if (a>0) printf "%.2fx", b/a; else print "-" }')
    printf '%-14s %7ss %7ss %7s\n' "$name" "$off" "$on" "$r"
done
