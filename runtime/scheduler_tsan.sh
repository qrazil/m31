#!/usr/bin/env bash
# ThreadSanitizer, for the Phase 2 scheduler specifically. NOT run by
# gates.sh and NOT run by runtime/scheduler_test.sh -- TSan instrumentation
# and ASan/UBSan instrumentation are mutually exclusive in one binary (TSan
# uses its own shadow-memory scheme, incompatible with ASan's), so this is
# its own separate build-and-run step, by design, the same way
# runtime/sanitize.sh's ASan+UBSan gate is already separate from an
# ordinary build.
#
# Why this matters more here than almost anywhere else in this codebase:
# ASan/UBSan (what the rest of this project uses) catch memory-safety and
# undefined-behaviour bugs, but NEITHER catches data races between threads.
# This scheduler is genuinely concurrent -- multiple real OS-thread carriers,
# a shared queue, per-carrier wake primitives, atomics everywhere -- so TSan
# is the one tool here that can actually find the class of bug this phase is
# most at risk of. It already found three real ones while this was being
# built (see scheduler.c's comments at total_completed/total_spawned/
# dispatched, and scheduler_test.c's comment on wake_worker's `done` store):
# all three were a counter read elsewhere being used as a "this already
# happened, so it's safe to look at what it produced" signal while using
# memory_order_relaxed, which provides no such guarantee -- only
# acquire/release does. Fixed by switching those specific counters to
# release (writer) / acquire (reader) pairs; nothing else changed.
#
# Usage: bash runtime/scheduler_tsan.sh
set -uo pipefail
cd "$(dirname "$0")/.."
. ./runtime/arch.sh

if ! command -v clang >/dev/null; then
    echo "skipped: clang not found (this host's gcc has no TSan runtime installed either)"
    exit 0
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

bin="$WORK/sched_tsan"
err="$WORK/cc.err"
if ! clang -O1 -g -fsanitize=thread -fno-omit-frame-pointer -Wall -Wextra \
        -I runtime -pthread \
        runtime/scheduler_test.c runtime/scheduler.c "$RT_REACTOR_C" runtime/rt.c \
        runtime/ctx_switch_x86_64.s -o "$bin" 2>"$err"; then
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
