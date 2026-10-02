#!/usr/bin/env bash
# ThreadSanitizer, for Phase 3 specifically (epoll reactor, park/unpark,
# blocking-FFI handoff) -- mirrors runtime/scheduler_tsan.sh exactly, for
# the identical reason: TSan and ASan/UBSan are mutually exclusive
# instrumentation modes, so this is its own separate build-and-run step, not
# run by gates.sh or by runtime/phase3_test.sh.
#
# This is, if anything, MORE load-bearing here than for Phase 2's own TSan
# gate: this phase adds a second mutex (rt_carrier_t.local_lock) explicitly
# shared between a carrier's own thread and the blocking-FFI monitor thread,
# a lock-free three-state CAS word (park_word) explicitly shared between a
# green thread's own call stack and an external unparking thread, and a
# mutex-protected registry whose whole job is making a cross-thread pointer
# dereference safe against a concurrent free(). ASan/UBSan (the ordinary
# gate) catch memory-safety and undefined-behaviour bugs; neither catches a
# data race. TSan is the one tool that can actually find the class of bug
# this phase is most at risk of -- exactly the race the design doc itself
# names as the most likely thing to be gotten wrong (docs/
# concurrency-decision.md, "What this costs").
#
# Usage: bash runtime/phase3_tsan.sh [runs]
set -uo pipefail
cd "$(dirname "$0")/.."
. ./runtime/arch.sh

if ! command -v clang >/dev/null; then
    echo "skipped: clang not found (this host's gcc has no TSan runtime installed either)"
    exit 0
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

bin="$WORK/phase3_tsan"
err="$WORK/cc.err"
if ! clang -O1 -g -fsanitize=thread -fno-omit-frame-pointer -Wall -Wextra \
        -I runtime -pthread \
        runtime/phase3_test.c runtime/scheduler.c runtime/reactor.c \
        runtime/rt.c "$RT_CTX_ASM" -o "$bin" 2>"$err"; then
    echo "TSan build FAILED:"
    sed 's/^/    /' "$err"
    exit 1
fi

fail=0
runs=${1:-5}
for i in $(seq 1 "$runs"); do
    out=$(TSAN_OPTIONS="halt_on_error=0 history_size=7" "$bin" 2>&1)
    rc=$?
    warnings=$(echo "$out" | grep -c "WARNING: ThreadSanitizer" || true)
    if [ "$rc" -eq 0 ] && [[ "$out" == *"0 failures"* ]] && [ "$warnings" -eq 0 ]; then
        echo "run $i/$runs: ok ($(echo "$out" | tail -1))"
    else
        echo "run $i/$runs: FAILED (exit=$rc, TSan warnings=$warnings)"
        echo "$out" | sed 's/^/    /'
        fail=1
    fi
done

exit $fail
