#!/usr/bin/env bash
# Heap watch's cost (docs/heap-watch-design.md, "Cost"): each program built
# twice -- the default build, which has no heap watch in it at all, and a
# heap watch build (-DRT_HEAPWATCH) -- and the heap watch build run in each
# mode: M31_HEAPWATCH=0, the sampled ledger (the default) and full (the
# ledger and epoch close).
#
# Interleaved, best of RUNS: the noise on this machine is other builds, and
# noise only ever adds. Compare the default build against a build of the
# commit before heap watch, too: that it costs nothing is a claim about it
# (the design doc's table), and gates.sh checks it has none of heap watch's
# code.
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

build() { # build <source> <name> [-DRT_HEAPWATCH]
    "$LANGC" -emit-c "$1" --out "$WORK/$2.c" || return 1
    "$CC" -O2 -ffp-contract=off ${3:-} -I runtime "$WORK/$2.c" \
        runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" \
        -lpthread -o "$WORK/$2.bin" || return 1
}

seconds() {
    { /usr/bin/time -f '%e' env "$@" >/dev/null; } 2>&1 | tail -1
}

ratio() { awk -v a="$1" -v b="$2" 'BEGIN{ if (a>0) printf "%+.0f%%", (b/a-1)*100; else print "-" }'; }

printf '%-14s %8s %16s %16s %16s\n' benchmark default "hw build, off" "ledger" "full"
for src in bench/trees/trees.m31 bench/heapwatch/allocs.m31 bench/heapwatch/stores.m31 \
           bench/valenum/result-boxed.m31 bench/valenum/option-boxed.m31; do
    name=$(basename "$src" ".$LANG_EXT")
    build "$src" "$name" || { echo "$name: build failed"; continue; }
    build "$src" "$name.hw" -DRT_HEAPWATCH || { echo "$name: heap watch build failed"; continue; }
    plain="" off="" ledger="" full=""
    best() { # best <var> <seconds>
        local cur=${!1}
        { [ -z "$cur" ] || awk -v a="$2" -v b="$cur" 'BEGIN{exit !(a<b)}'; } && printf -v "$1" '%s' "$2"
    }
    for _ in $(seq "$RUNS"); do
        best plain "$(seconds "$WORK/$name.bin")"
        best off "$(seconds M31_HEAPWATCH=0 "$WORK/$name.hw.bin")"
        best ledger "$(seconds M31_HEAPWATCH=ledger "$WORK/$name.hw.bin")"
        best full "$(seconds M31_HEAPWATCH=full "$WORK/$name.hw.bin")"
    done
    printf '%-14s %7ss %9ss %5s %9ss %5s %9ss %5s\n' "$name" "$plain" \
        "$off" "$(ratio "$plain" "$off")" \
        "$ledger" "$(ratio "$plain" "$ledger")" \
        "$full" "$(ratio "$plain" "$full")"
done
