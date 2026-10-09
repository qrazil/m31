#!/usr/bin/env bash
# The corpus programs that exercise parked, bounded waits, run on ONE and on
# TWO carriers (the corpus itself runs at the machine's default).
#
# stdlib-timer: 1000 sleepers of 300 ms must be awake in under four
# intervals. stdlib-net-timeout-park: 150 silent connections with a one
# second read timeout must not delay a live connection beside them. Each
# prints facts, not numbers, so the expected output is the corpus's own
# main.out. If a wait held its carrier, a program on one or two carriers
# would take minutes, and its bound would fail.
#
# Usage: bash runtime/parked_waits_carriers_test.sh
set -uo pipefail
cd "$(dirname "$0")/.."
. ./config.sh
. ./runtime/arch.sh

LANGC=${LANGC:-./target/debug/$LANG_BIN}
if [ ! -x "$LANGC" ]; then
    echo "compiler not built: $LANGC" >&2
    exit 1
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
fail=0

for prog in stdlib-timer stdlib-net-timeout-park; do
    dir=corpus/modules/$prog
    if ! "$LANGC" --emit-c "$dir/main.m31" -o "$WORK/$prog.c" 2>"$WORK/$prog.err"; then
        echo "FAILED: compile $prog: $(grep -v '^warning' "$WORK/$prog.err" | head -5)"
        fail=1
        continue
    fi
    if ! cc -O2 -pthread -I runtime -o "$WORK/$prog" "$WORK/$prog.c" \
            runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" \
            2>"$WORK/$prog.cc"; then
        echo "FAILED: build $prog:"
        sed 's/^/    /' "$WORK/$prog.cc" | head -20
        fail=1
        continue
    fi
    for carriers in 1 2; do
        start=$SECONDS
        out=$(LANG_NUM_CARRIERS=$carriers bash runtime/with_timeout.sh 60 "$WORK/$prog" 2>&1)
        rc=$?
        took=$((SECONDS - start))
        if [ "$rc" -eq 0 ] && [ "$out" = "$(cat "$dir/main.out")" ]; then
            echo "ok: $prog, LANG_NUM_CARRIERS=$carriers (${took}s)"
        else
            echo "FAILED: $prog, LANG_NUM_CARRIERS=$carriers (exit=$rc, ${took}s)"
            diff <(echo "$out") "$dir/main.out" | head -10 | sed 's/^/    /'
            fail=1
        fi
    done
done
exit $fail
