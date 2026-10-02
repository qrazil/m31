#!/usr/bin/env bash
# Tests for Phase 2 of docs/concurrency-decision.md: the green-thread
# scheduler (runtime/scheduler.c/.h), built on Phase 1's primitives.
#
# Mirrors runtime/greenthread_test.sh's shape: build runtime/scheduler_test.c
# (together with scheduler.c, rt.c and ctx_switch_x86_64.s -- this scheduler
# is its own standalone component, never #included by rt.c, so every build
# line here names all four files explicitly) against every compiler/opt
# combination gates.sh already checks the runtime with, run it, and
# additionally run one dedicated ASan+UBSan build.
#
# ThreadSanitizer is NOT run by this script or by gates.sh -- TSan and
# ASan/UBSan are mutually exclusive instrumentation modes that cannot be
# linked into the same binary, and this project's existing convention
# (sanitize.sh, greenthread_test.sh) is already an ASan+UBSan gate. Run
# runtime/scheduler_tsan.sh by hand to build and run the dedicated
# TSan-instrumented variant; see that file's header for how and why.
set -uo pipefail
cd "$(dirname "$0")/.."
. ./config.sh
. ./runtime/arch.sh

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
fail=0

note() { printf '%-56s %s\n' "$1" "$2"; }

# rt.c unconditionally calls into the epoll reactor now too (lib/net.src's
# `__wait_io` -> rt_wait_io -> rt_global_reactor/rt_reactor_wait), even
# though this Phase 2 test never exercises that path, so reactor.c joins
# the link line alongside scheduler.c.
SRCS="runtime/scheduler_test.c runtime/scheduler.c runtime/reactor.c runtime/rt.c $RT_CTX_ASM"

for cc in gcc clang; do
    command -v "$cc" >/dev/null || continue
    for opt in -O0 -O2; do
        label="$cc $opt"
        bin="$WORK/sched_${cc}_${opt#-}"
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
            note "scheduler suite [$label]" "ok ($(echo "$out" | tail -1))"
        else
            note "scheduler suite [$label]" FAILED
            echo "$out" | sed 's/^/    /' | head -40
            fail=1
        fi

        yout=$("$bin" yield_outside 2>&1); rc=$?
        if [ "$rc" -eq 134 ] && echo "$yout" | grep -q "outside a running green thread"; then
            note "yield-outside trap [$label]" "ok (exit 134, correct message)"
        else
            note "yield-outside trap [$label]" FAILED
            echo "$yout" | sed 's/^/    /' | tail -10
            echo "    exit=$rc"
            fail=1
        fi
    done
done

# ---- ASan + UBSan, once -----------------------------------------------
# clang only, same reasoning as greenthread_test.sh and sanitize.sh: this
# host's gcc has no sanitizer runtime installed.
SAN="-fsanitize=address,undefined -fno-sanitize-recover=all -fno-omit-frame-pointer"
if command -v clang >/dev/null && \
        echo 'int main(void){return 0;}' >"$WORK/probe.c" && \
        clang $SAN "$WORK/probe.c" -o "$WORK/probe" 2>/dev/null; then
    sbin="$WORK/sched_san"
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
            note "sanitized scheduler suite" "ok ($(echo "$sout" | tail -1))"
        else
            note "sanitized scheduler suite" FAILED
            echo "$sout" | sed 's/^/    /' | head -40
            fail=1
        fi

        syout=$("$sbin" yield_outside 2>&1); rc=$?
        if [ "$rc" -eq 134 ] && echo "$syout" | grep -q "outside a running green thread" \
                && ! echo "$syout" | grep -qE 'ERROR: (AddressSanitizer|LeakSanitizer)|runtime error:'; then
            note "sanitized yield-outside trap" "ok (exit 134, clean trap)"
        else
            note "sanitized yield-outside trap" FAILED
            echo "$syout" | sed 's/^/    /' | tail -10
            fail=1
        fi
    fi
else
    note "sanitized build" "skipped (clang cannot link a sanitized program here)"
fi

exit $fail
