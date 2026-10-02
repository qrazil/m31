#!/usr/bin/env bash
# Regression test for the `rt_sched_spawn` backpressure deadlock
# (docs/concurrency-decision.md, "Phase 3.5"): `scheduler.c`'s `squeue_push`
# used to block a full shared queue with a real `pthread_cond_wait`
# regardless of who called it, which deadlocks the instant the caller is a
# green thread running on the only carrier that could ever drain the
# queue -- that carrier is now blocked waiting on itself, forever.
#
# Fixed by having `squeue_push` park the calling green thread
# (`rt_sched_park(RT_GT_PARKED_QUEUE)`) instead of blocking the carrier when
# the caller IS one -- the same shape `Chan.send`/`recv` already use.
#
# runtime/spawn_backpressure_demo/main.src spawns 300 workers in a tight
# loop from the top level (which always runs as green thread 0,
# rt_run_program) over a channel of capacity 300, so none of them can block
# on the channel itself -- the only thing that can possibly block here is
# the scheduler's own shared queue. Run with LANG_GLOBAL_QUEUE_CAP=4 so that
# cap is reached on the fourth spawn rather than only past the production
# default of a million, which this bug's own fix made irrelevant to safety
# but would make irrelevant to testing too if left at its default (the test
# would "pass" by never actually reaching the backpressure path at all).
#
# Usage: bash runtime/spawn_backpressure_test.sh [stock-runs] [tsan-runs]
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

stock_runs=${1:-40}
tsan_runs=${2:-15}
fail=0
EXPECTED=44850 # sum(0..299) -- see main.src's own comment

SRC=runtime/spawn_backpressure_demo/main.src
C="$WORK/sb.c"

if ! "$LANGC" --emit-c "$SRC" -o "$C" 2>"$WORK/compile.err"; then
    echo "FAILED: compile: $(head -5 "$WORK/compile.err")"
    exit 1
fi

# ---- stock build: every carrier count the deadlock could plausibly hide
# behind (1 is the exact reported shape; 2 and 4 confirm the fix does not
# regress the ordinary multi-carrier case) -------------------------------
STOCK="$WORK/sb.stock"
if ! cc -O2 -pthread -I runtime -o "$STOCK" "$C" \
        runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" \
        "$RT_CTX_ASM" 2>"$WORK/stock.err"; then
    echo "FAILED: stock build:"
    sed 's/^/    /' "$WORK/stock.err" | head -20
    exit 1
fi

for carriers in 1 2 4; do
    ok=0
    for i in $(seq 1 "$stock_runs"); do
        out=$(LANG_NUM_CARRIERS=$carriers LANG_GLOBAL_QUEUE_CAP=4 \
              timeout 15 "$STOCK" 2>&1)
        rc=$?
        if [ "$rc" -eq 0 ] && [ "$out" = "$EXPECTED" ]; then
            ok=$((ok + 1))
        else
            echo "FAILED: stock carriers=$carriers run $i/$stock_runs (exit=$rc)"
            echo "$out" | sed 's/^/    /' | head -10
            fail=1
            break
        fi
    done
    [ "$ok" -eq "$stock_runs" ] && \
        echo "ok: stock, LANG_NUM_CARRIERS=$carriers ($ok/$stock_runs clean runs, correct sum)"
done

# ---- TSan build: the same scenario, at the one carrier count the bug
# actually needs (1) and one that exercises genuine cross-carrier draining
# of the parked waiter (2) -------------------------------------------------
if command -v clang >/dev/null; then
    TSAN="$WORK/sb.tsan"
    if clang -O1 -g -fsanitize=thread -fno-omit-frame-pointer -Wall -Wextra \
            -I runtime -pthread "$C" runtime/rt.c runtime/scheduler.c \
            "$RT_REACTOR_C" "$RT_CTX_ASM" -o "$TSAN" \
            2>"$WORK/tsan.err"; then
        for carriers in 1 2; do
            ok=0
            for i in $(seq 1 "$tsan_runs"); do
                out=$(LANG_NUM_CARRIERS=$carriers LANG_GLOBAL_QUEUE_CAP=4 \
                      TSAN_OPTIONS="halt_on_error=0 history_size=7" \
                      timeout 20 "$TSAN" 2>&1)
                rc=$?
                warnings=$(echo "$out" | grep -c "WARNING: ThreadSanitizer" || true)
                got=$(echo "$out" | grep -v "WARNING: ThreadSanitizer" | grep -v '^    #[0-9]')
                if [ "$rc" -eq 0 ] && [ "$warnings" -eq 0 ] && [ "$got" = "$EXPECTED" ]; then
                    ok=$((ok + 1))
                else
                    echo "FAILED: tsan carriers=$carriers run $i/$tsan_runs (exit=$rc, TSan warnings=$warnings)"
                    echo "$out" | sed 's/^/    /' | head -30
                    fail=1
                    break
                fi
            done
            [ "$ok" -eq "$tsan_runs" ] && \
                echo "ok: tsan, LANG_NUM_CARRIERS=$carriers ($ok/$tsan_runs clean runs, no races, correct sum)"
        done
    else
        echo "FAILED: TSan build:"
        sed 's/^/    /' "$WORK/tsan.err" | head -20
        fail=1
    fi
else
    echo "skipped: clang not found, TSan runs not performed"
fi

exit $fail
