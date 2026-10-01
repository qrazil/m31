#!/usr/bin/env bash
# Benchmarks, against the languages this design is actually competing with.
#
# Why these: the point of the memory model is deterministic destruction with
# no collector, so the benchmarks that matter are the ones that allocate and
# free. C is the floor (malloc/free by hand), Go is a tracing collector at
# the same altitude, Rust is ownership with no counting at all, and Python is
# refcounting in an interpreter.
#
# Nim with --mm:arc would be the closest comparison of all -- compiled,
# non-atomic refcounting, emits C -- and is not installed on this machine.
#
# Each program is run WARMUP+RUNS times and the best wall clock is kept; the
# best, not the mean, because the noise here is other processes, and noise
# only ever adds.
set -uo pipefail
cd "$(dirname "$0")/.."
. ./config.sh

LANGC=${LANGC:-./target/debug/$LANG_BIN}
RUNS=${RUNS:-5}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

best() {                      # best wall clock of RUNS, in seconds
    local b=""
    for _ in $(seq "$RUNS"); do
        local t
        t=$( { /usr/bin/time -f '%e' "$@" >/dev/null; } 2>&1 | tail -1 )
        [ -z "$b" ] && b=$t
        awk -v a="$t" -v b="$b" 'BEGIN{exit !(a<b)}' && b=$t
    done
    echo "$b"
}

peak() {                      # peak resident set, in KB
    { /usr/bin/time -f '%M' "$@" >/dev/null; } 2>&1 | tail -1
}

printf '%-14s %10s %10s %10s %10s\n' benchmark ours C Go Python
for dir in bench/*/; do
    name=$(basename "$dir")
    [ -e "$dir/$name.$LANG_EXT" ] || continue

    "$LANGC" --emit-c "$dir/$name.$LANG_EXT" -o "$WORK/$name.c" || continue
    gcc -O2 -ffp-contract=off -I runtime "$WORK/$name.c" \
        runtime/rt.c runtime/scheduler.c runtime/reactor.c runtime/ctx_switch_x86_64.s \
        -lpthread -o "$WORK/$name.ours" || continue
    ours=$(best "$WORK/$name.ours")
    mem=$(peak "$WORK/$name.ours")

    c="-"; [ -e "$dir/$name.c" ] && gcc -O2 "$dir/$name.c" -o "$WORK/$name.c.bin" \
        && c=$(best "$WORK/$name.c.bin")
    go="-"; [ -e "$dir/$name.go" ] && command -v go >/dev/null \
        && go build -o "$WORK/$name.go.bin" "$dir/$name.go" 2>/dev/null \
        && go=$(best "$WORK/$name.go.bin")
    py="-"; [ -e "$dir/$name.py" ] && command -v python3 >/dev/null \
        && py=$(best python3 "$dir/$name.py")

    printf '%-14s %10s %10s %10s %10s   (%s KB)\n' "$name" "$ours" "$c" "$go" "$py" "$mem"
done
