#!/usr/bin/env bash
# Tests for Phase 3 of docs/concurrency-decision.md: epoll reactor,
# park/unpark, blocking-FFI handoff (runtime/scheduler.c/.h's additive Phase
# 3 surface, runtime/reactor.h). Built on the Phase 2 scheduler and Phase
# 1's primitives, exactly mirroring runtime/scheduler_test.sh's own shape:
# build runtime/phase3_test.c against every compiler/opt combination
# gates.sh already checks the runtime with, run it, and additionally run
# one dedicated ASan+UBSan build.
#
# ThreadSanitizer is NOT run by this script or by gates.sh -- see
# runtime/phase3_tsan.sh, run by hand, for that (same reasoning as
# scheduler_tsan.sh: TSan and ASan/UBSan cannot share one binary).
set -uo pipefail
cd "$(dirname "$0")/.."
. ./config.sh
. ./runtime/arch.sh

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
fail=0

note() { printf '%-56s %s\n' "$1" "$2"; }

SRCS="runtime/phase3_test.c runtime/scheduler.c $RT_REACTOR_C runtime/rt.c runtime/ctx_switch_x86_64.s"

for cc in gcc clang; do
    command -v "$cc" >/dev/null || continue
    for opt in -O0 -O2; do
        label="$cc $opt"
        bin="$WORK/p3_${cc}_${opt#-}"
        err="$WORK/cc.err"
        if ! "$cc" "$opt" -Wall -Wextra -I runtime -pthread \
                $SRCS -o "$bin" 2>"$err" || [ -s "$err" ]; then
            note "build [$label]" FAILED
            sed 's/^/    /' "$err" | head -20
            fail=1
            continue
        fi

        out=$("$bin" 2>&1); rc=$?
        if [ "$rc" -eq 0 ] && [[ "$out" == *"0 failures"* ]]; then
            note "phase3 suite [$label]" "ok ($(echo "$out" | tail -1))"
        else
            note "phase3 suite [$label]" FAILED
            echo "$out" | sed 's/^/    /' | head -60
            fail=1
        fi
    done
done

# ---- ASan + UBSan, once -----------------------------------------------
SAN="-fsanitize=address,undefined -fno-sanitize-recover=all -fno-omit-frame-pointer"
if command -v clang >/dev/null && \
        echo 'int main(void){return 0;}' >"$WORK/probe.c" && \
        clang $SAN "$WORK/probe.c" -o "$WORK/probe" 2>/dev/null; then
    sbin="$WORK/p3_san"
    serr="$WORK/san.err"
    if ! clang -O1 -g $SAN -Wall -Wextra -I runtime -pthread \
            $SRCS -o "$sbin" 2>"$serr"; then
        note "sanitized build" FAILED
        sed 's/^/    /' "$serr" | head -20
        fail=1
    else
        sout=$("$sbin" 2>&1); rc=$?
        if [ "$rc" -eq 0 ] && [[ "$sout" == *"0 failures"* ]] && \
                ! echo "$sout" | grep -qE 'ERROR: (AddressSanitizer|LeakSanitizer)|runtime error:'; then
            note "sanitized phase3 suite" "ok ($(echo "$sout" | tail -1))"
        else
            note "sanitized phase3 suite" FAILED
            echo "$sout" | sed 's/^/    /' | head -60
            fail=1
        fi
    fi
else
    note "sanitized build" "skipped (clang cannot link a sanitized program here)"
fi

exit $fail
