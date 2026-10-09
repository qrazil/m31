#!/usr/bin/env bash
# The stall measurement behind docs/net-timeouts-decision.md, as a test.
#
# runtime/net_timeout_stall_demo/main.m31 opens N connections that never say
# anything, gives every one a 400 ms read timeout, and then serves one more
# connection that does. Run on TWO carriers, N = 2 and N = 16. While a
# timeout was a ppoll(2) on the carrier, N silent connections pinned both
# carriers for the whole timeout and the live one waited behind them:
#
#     silent=2   live_ms=521    (ideal 120: +401)
#     silent=16  live_ms=3327   (ideal 120: +3207)
#
# Now a timeout parks only the green thread, and the live connection is
# served at once. This asserts that, with a bound of timeout + 150 ms over
# the ideal -- generous on purpose, a loaded machine must not fail it, and
# the stall it guards against is 400 ms and up.
#
# Usage: bash runtime/net_timeout_stall_test.sh [runs]
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
runs=${1:-3}
fail=0

IDEAL_MS=120       # two 60 ms settling pauses in main.m31
TIMEOUT_MS=400
SLACK_MS=150

SRC=runtime/net_timeout_stall_demo/main.m31
if ! "$LANGC" --emit-c "$SRC" -o "$WORK/stall.c" 2>"$WORK/compile.err"; then
    echo "FAILED: compile: $(grep -v '^warning' "$WORK/compile.err" | head -5)"
    exit 1
fi
if ! cc -O2 -pthread -I runtime -o "$WORK/stall" "$WORK/stall.c" \
        runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" \
        2>"$WORK/cc.err"; then
    echo "FAILED: build:"
    sed 's/^/    /' "$WORK/cc.err" | head -20
    exit 1
fi

worst_live=0
for i in $(seq 1 "$runs"); do
    out=$(LANG_NUM_CARRIERS=2 bash runtime/with_timeout.sh 60 "$WORK/stall" 2>&1)
    rc=$?
    lines=$(echo "$out" | grep -c '^silent=')
    if [ "$rc" -ne 0 ] || [ "$lines" -ne 2 ]; then
        echo "FAILED: run $i/$runs (exit=$rc)"
        echo "$out" | sed 's/^/    /' | head -10
        fail=1
        continue
    fi
    while read -r line; do
        n=$(echo "$line" | sed -E 's/.*silent=([0-9]+).*/\1/')
        live=$(echo "$line" | sed -E 's/.*live_ms=([0-9]+).*/\1/')
        smax=$(echo "$line" | sed -E 's/.*silent_max_ms=([0-9]+).*/\1/')
        [ "$live" -gt "$worst_live" ] && worst_live=$live
        if [ "$live" -ge $((IDEAL_MS + SLACK_MS)) ]; then
            echo "FAILED: run $i, $n silent: live connection took ${live} ms" \
                 "(bound $((IDEAL_MS + SLACK_MS)): it stalled behind the silent ones)"
            fail=1
        fi
        if [ "$smax" -lt $((TIMEOUT_MS - 5)) ] || \
           [ "$smax" -ge $((TIMEOUT_MS + SLACK_MS)) ]; then
            echo "FAILED: run $i, $n silent: slowest timeout took ${smax} ms" \
                 "(want ${TIMEOUT_MS}..$((TIMEOUT_MS + SLACK_MS)))"
            fail=1
        fi
    done < <(echo "$out" | grep '^silent=')
done

if [ "$fail" -eq 0 ]; then
    echo "ok: 2 carriers, 400 ms timeout, 2 and 16 silent clients, $runs runs; worst live connection ${worst_live} ms (bound $((IDEAL_MS + SLACK_MS)))"
fi
exit $fail
