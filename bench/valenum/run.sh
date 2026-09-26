#!/usr/bin/env bash
# Value enums, before and after -- in one compiler.
#
# Each pair is the same program twice, the second with one extra line that
# puts the enum in a `List` and nothing else. A collection's element is one
# machine word, so that line alone takes the enum back to being a heap object
# for the whole program (docs/value-enums.md §2) while leaving the hot loop
# byte for byte identical. The difference between the two timings is the
# allocation, the free and the refcount traffic -- which is what the program
# cost before value enums existed.
#
# Best of RUNS, not the mean: the noise on this machine is other builds, and
# noise only ever adds.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
. ./config.sh

LANGC=${LANGC:-./target/debug/$LANG_BIN}
CC=${CC:-gcc}
RUNS=${RUNS:-7}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

build() {
    "$LANGC" --emit-c "bench/valenum/$1.$LANG_EXT" -o "$WORK/$1.c" || return 1
    "$CC" -O2 -ffp-contract=off -I runtime "$WORK/$1.c" runtime/rt.c -lpthread \
        -o "$WORK/$1.bin" || return 1
}

best() {
    local b="" t
    for _ in $(seq "$RUNS"); do
        t=$( { /usr/bin/time -f '%e' "$1" >/dev/null; } 2>&1 | tail -1 )
        [ -z "$b" ] && b=$t
        awk -v a="$t" -v b="$b" 'BEGIN{exit !(a<b)}' && b=$t
    done
    echo "$b"
}

# Allocations, counted rather than guessed. `runtime/rc_debug.h` keeps a
# running total beside the live count and prints it under -DRC_COUNT_ALLOCS,
# on a line of its own so the corpus's output is unchanged.
allocs() {
    "$LANGC" --emit-c "bench/valenum/$1.$LANG_EXT" -o "$WORK/$1.dbg.c" || return 1
    "$CC" -O2 -DRC_DEBUG -DRC_COUNT_ALLOCS -I runtime "$WORK/$1.dbg.c" runtime/rt.c \
        -lpthread -o "$WORK/$1.dbg" 2>/dev/null || { echo "-"; return; }
    "$WORK/$1.dbg" 2>/dev/null | sed -n 's/^__rc_allocs=//p'
}

printf '%-10s %9s %9s %7s %14s %14s\n' pair value boxed ratio 'allocs value' 'allocs boxed'
for name in option result16 result; do
    build "$name" || { echo "$name: build failed"; continue; }
    build "$name-boxed" || { echo "$name-boxed: build failed"; continue; }
    a=$(best "$WORK/$name.bin")
    b=$(best "$WORK/$name-boxed.bin")
    r=$(awk -v a="$a" -v b="$b" 'BEGIN{ if (a>0) printf "%.2fx", b/a; else print "-" }')
    na=$(allocs "$name")
    nb=$(allocs "$name-boxed")
    printf '%-10s %8ss %8ss %7s %14s %14s\n' "$name" "$a" "$b" "$r" "$na" "$nb"
done
