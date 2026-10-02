#!/usr/bin/env bash
# ThreadSanitizer for the Phase 3.5 wiring (docs/concurrency-decision.md,
# "Phase 3.5"): `spawn` always a green thread, `Chan.send`/`recv` parking
# with a FIFO multi-waiter queue, and lib/net.m31's sockets going through
# the non-blocking-plus-reactor path. scheduler_tsan.sh and phase3_tsan.sh
# already cover the STANDALONE scheduler/reactor via their own hand-written
# C harnesses; this script covers the actual integration, which is only
# reachable by compiling real `.m31` programs through `langc` -- the thing
# neither of those harnesses exercises at all.
#
# Mirrors their own shape and reasoning exactly (see their headers for the
# fuller argument for why TSan, not ASan/UBSan, is the tool that can find
# this class of bug): build each test program with clang+TSan, linking
# runtime/rt.c/scheduler.c/reactor.c/ctx_switch_x86_64.S (the same four
# files every ordinary build now links), run it N times.
#
# TWO DIFFERENT EXPECTATIONS, NOT ONE -- read this before changing either:
#
#   1. The `Chan` tests (corpus/core/1303, 1304: hundreds of green threads
#      contending on a channel's send/recv waiter queues) are expected to
#      run CLEAN, every time -- `send`/`recv` are compiler intrinsics, never
#      `prim` calls, so they never touch the code path below.
#
#   2. The real-socket-I/O test (runtime/net_concurrency_demo/main.m31) is
#      run and its result is REPORTED, not gated on passing clean: while
#      building this integration, running this exact program under this
#      exact script caught a genuine, pre-existing data race on
#      `rt_stack_limit` between two carrier OS threads (one writing it in
#      `rt_fiber_switch`, runtime/greenthread.h; the other reading it in a
#      green thread's own compiler-emitted stack probe) -- a bug in Phase
#      1/2's own fiber-switching machinery, not in anything this task added,
#      never caught by scheduler_tsan.sh/phase3_tsan.sh because their own
#      test patterns never drove genuinely concurrent multi-carrier fiber
#      dispatch at the rate real concurrent socket I/O does. It was not
#      fixed as part of this task (see runtime/greenthread.h's
#      RT_STACK_SIZE comment and runtime/net_concurrency_demo/README.md for
#      the full account and why). This script keeps running that case
#      specifically so the race stays reproducible and visible -- silently
#      dropping it would hide a real, serious, open bug.
#
# Carrier counts differ per test, deliberately -- see each function's own
# comment below for why: the `Chan` tests run under LANG_NUM_CARRIERS=1 to
# dodge a known TSan-only artifact (Phase 3's own documented gap,
# reconfirmed here); the net test runs under LANG_NUM_CARRIERS=2 because
# single-carrier would hide the real bug this script exists to keep
# reproducible. Neither choice is about the single-carrier spawn-
# backpressure deadlock documented in runtime/rt.c's init_global_scheduler
# (mitigated, not fixed, there) -- a separate, already-understood
# limitation, not a TSan question.
#
# Usage: bash runtime/spawn_wiring_tsan.sh [chan-runs] [net-runs]
set -uo pipefail
cd "$(dirname "$0")/.."
. ./config.sh
. ./runtime/arch.sh

if ! command -v clang >/dev/null; then
    echo "skipped: clang not found (this host's gcc has no TSan runtime installed either)"
    exit 0
fi

LANGC=${LANGC:-./target/debug/$LANG_BIN}
if [ ! -x "$LANGC" ]; then
    echo "compiler not built: $LANGC" >&2
    exit 1
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

chan_runs=${1:-30}
net_runs=${2:-5}
fail=0

build() {
    local src=$1 label=$2
    local c="$WORK/$label.c" bin="$WORK/$label.bin" err="$WORK/$label.cc"
    if ! "$LANGC" --emit-c "$src" -o "$c" 2>"$WORK/$label.diag"; then
        echo "FAILED: $label: compile: $(head -3 "$WORK/$label.diag")"
        return 1
    fi
    if ! clang -O1 -g -fsanitize=thread -fno-omit-frame-pointer -Wall -Wextra \
            -I runtime -pthread \
            "$c" runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" \
            "$RT_CTX_ASM" -o "$bin" 2>"$err"; then
        echo "FAILED: $label: TSan build:"
        sed 's/^/    /' "$err" | head -20
        return 1
    fi
    echo "$bin"
}

# gate <bin> <expected-stdout> <label> <runs> -- must pass every run.
#
# LANG_NUM_CARRIERS=1 here, not 2 -- this hit exactly the Phase 3 "Known
# gap" documented in docs/concurrency-decision.md's Progress section
# (phase3_test.c's own comment has the full account): TSan itself
# intermittently segfaults inside its OWN StackDepotBase::Put/CurrentStackId
# machinery when a green thread is suspended by one carrier and resumed by
# a different one, under TSan specifically -- never a real race report,
# never in this project's own code, confirmed by Phase 3 and reconfirmed
# here. Phase 3's own fix was narrowing its two affected tests to 1 carrier
# under TSan only; this script hit the identical signature (same crash
# site, same trigger shape: many cross-carrier park/unpark cycles) on these
# two NEW tests and applies the SAME documented mitigation rather than
# re-deriving it. Every plain (non-TSan) build already exercises these at
# full multi-carrier counts with no issue (see corpus/core/1303, 1304).
gate() {
    local bin=$1 expected=$2 label=$3 runs=$4
    local i ok=0
    for i in $(seq 1 "$runs"); do
        out=$(LANG_NUM_CARRIERS=1 TSAN_OPTIONS="halt_on_error=0 history_size=7" \
              timeout 20 "$bin" 2>&1)
        rc=$?
        warnings=$(echo "$out" | grep -c "WARNING: ThreadSanitizer" || true)
        got=$(echo "$out" | grep -v "WARNING: ThreadSanitizer" | grep -v '^    #[0-9]')
        if [ "$rc" -eq 0 ] && [ "$warnings" -eq 0 ] && [ "$got" = "$expected" ]; then
            ok=$((ok + 1))
        else
            echo "FAILED: $label run $i/$runs (exit=$rc, TSan warnings=$warnings)"
            echo "$out" | sed 's/^/    /' | head -40
            fail=1
            break
        fi
    done
    [ "$ok" -eq "$runs" ] && echo "ok: $label ($ok/$runs clean runs, LANG_NUM_CARRIERS=1 -- Phase 3 TSan-segfault mitigation)"
}

# report <bin> <label> <runs> -- runs and reports; never fails the script.
report() {
    local bin=$1 label=$2 runs=$3
    local i clean=0 raced=0 crashed_other=0
    for i in $(seq 1 "$runs"); do
        out=$(LANG_NUM_CARRIERS=2 TSAN_OPTIONS="halt_on_error=0 history_size=7" \
              timeout 20 "$bin" 2>&1)
        rc=$?
        if echo "$out" | grep -q "WARNING: ThreadSanitizer: data race"; then
            raced=$((raced + 1))
        elif [ "$rc" -ne 0 ]; then
            crashed_other=$((crashed_other + 1))
        else
            clean=$((clean + 1))
        fi
    done
    echo "report: $label -- $clean/$runs clean, $raced/$runs TSan data-race reports, $crashed_other/$runs other non-zero exit (see runtime/net_concurrency_demo/README.md)"
}

if bin=$(build corpus/core/1303-many-receivers-wait-on-one-channel.m31 chan_many_receivers); then
    gate "$bin" 89700 chan_many_receivers "$chan_runs"
else
    fail=1
fi

if bin=$(build corpus/core/1304-many-senders-wait-on-one-channel.m31 chan_many_senders); then
    gate "$bin" 44850 chan_many_senders "$chan_runs"
else
    fail=1
fi

if bin=$(build runtime/net_concurrency_demo/main.m31 net_concurrent_clients); then
    report "$bin" net_concurrent_clients "$net_runs"
else
    fail=1
fi

exit $fail
