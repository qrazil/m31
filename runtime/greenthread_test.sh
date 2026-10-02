#!/usr/bin/env bash
# Tests for Phase 1 of docs/concurrency-decision.md (x86-64 context switch,
# slab stack allocator, the probe, the per-thread state table).
#
# Mirrors runtime/sys_test.sh's shape: build runtime/greenthread_test.c
# against every compiler/opt combination gates.sh already checks the
# runtime with, run it, and additionally run the two failure-mode
# sub-tests (which are MEANT to abort -- SIGABRT, exit 134) as separate
# processes, plus one dedicated ASan+UBSan build of everything.
#
# A fourth check does not touch this binary at all: it compiles a trivial
# .src program with the actual langc binary and greps the emitted C for a
# call to rt_stack_check() -- the compiler side of the Part 3 contract,
# which runtime/greenthread_test.c cannot reach (see that file's header
# comment for why). MUST be a real call, never the probe's comparison text
# inlined directly at the call site -- see rt_stack_limit's own comment in
# rt.h for the real, reproduced bug that shape causes under cross-carrier
# park/resume.
set -uo pipefail
cd "$(dirname "$0")/.."
. ./config.sh
. ./runtime/arch.sh

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
fail=0

note() { printf '%-56s %s\n' "$1" "$2"; }

# ---- 1. functional suite + failure modes, every compiler/opt combo --------
for cc in gcc clang; do
    command -v "$cc" >/dev/null || continue
    for opt in -O0 -O2; do
        label="$cc $opt"
        bin="$WORK/gt_${cc}_${opt#-}"
        err="$WORK/cc.err"
        # rt.c unconditionally calls into the scheduler/reactor now (`spawn`
        # and `Chan` always route through them), so every binary that links
        # rt.c -- this hand-written C harness included -- links those two
        # translation units too, even though this particular test never
        # exercises them.
        if ! "$cc" "$opt" -Wall -Wextra -I runtime -pthread \
                runtime/greenthread_test.c runtime/rt.c \
                runtime/scheduler.c "$RT_REACTOR_C" \
                "$RT_CTX_ASM" -o "$bin" 2>"$err" || [ -s "$err" ]; then
            note "build [$label]" FAILED
            sed 's/^/    /' "$err" | head -20
            fail=1
            continue
        fi

        out=$("$bin" 2>&1); rc=$?
        if [ "$rc" -eq 0 ] && [[ "$out" == *"0 failures"* ]]; then
            note "main suite [$label]" "ok ($out)"
        else
            note "main suite [$label]" FAILED
            echo "$out" | sed 's/^/    /' | head -20
            fail=1
        fi

        ofout=$("$bin" overflow 2>&1); rc=$?
        depth=$(echo "$ofout" | grep -o 'depth [0-9]*' | tail -1 | awk '{print $2}')
        if [ "$rc" -eq 134 ] && echo "$ofout" | grep -q "^trap: stack overflow$" \
                && [ -n "$depth" ] && [ "$depth" -ge 5 ] && [ "$depth" -le 100000 ]; then
            note "overflow trap [$label]" "ok (trapped at depth $depth, exit 134)"
        else
            note "overflow trap [$label]" FAILED
            echo "$ofout" | sed 's/^/    /' | tail -10
            echo "    exit=$rc depth=${depth:-?}"
            fail=1
        fi

        poout=$("$bin" poison 2>&1); rc=$?
        if [ "$rc" -eq 134 ] && echo "$poout" | grep -q "no scheduler exists yet"; then
            note "poison trap [$label]" "ok (exit 134, correct message)"
        else
            note "poison trap [$label]" FAILED
            echo "$poout" | sed 's/^/    /' | tail -10
            fail=1
        fi
    done
done

# ---- 2. ASan + UBSan, once -------------------------------------------------
# clang only: matches sanitize.sh's own reasoning (this host's gcc has no
# sanitizer runtime installed). If clang cannot even link a sanitized
# program here, the check is skipped and says so.
SAN="-fsanitize=address,undefined -fno-sanitize-recover=all -fno-omit-frame-pointer"
if command -v clang >/dev/null && \
        echo 'int main(void){return 0;}' >"$WORK/probe.c" && \
        clang $SAN "$WORK/probe.c" -o "$WORK/probe" 2>/dev/null; then
    sbin="$WORK/gt_san"
    serr="$WORK/san.err"
    if ! clang -O1 -g $SAN -Wall -Wextra -I runtime -pthread \
            runtime/greenthread_test.c runtime/rt.c \
            runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" \
            -o "$sbin" 2>"$serr"; then
        note "sanitized build" FAILED
        sed 's/^/    /' "$serr" | head -20
        fail=1
    else
        sout=$("$sbin" 2>&1); rc=$?
        if [ "$rc" -eq 0 ] && [[ "$sout" == *"0 failures"* ]] && \
                ! echo "$sout" | grep -qE 'ERROR: (AddressSanitizer|LeakSanitizer)|runtime error:'; then
            note "sanitized main suite" "ok ($sout)"
        else
            note "sanitized main suite" FAILED
            echo "$sout" | sed 's/^/    /' | head -30
            fail=1
        fi

        sofout=$("$sbin" overflow 2>&1); rc=$?
        # A clean trap means: it aborted (134), our own message is present,
        # and -- the actual point of running this under ASan -- no sanitizer
        # ERROR fired. The __asan_handle_no_return WARNING is expected and
        # is not a failure; see the note above RT_STACK_SIZE in
        # runtime/greenthread.h for exactly why it is benign here.
        if [ "$rc" -eq 134 ] && echo "$sofout" | grep -q "^trap: stack overflow$" \
                && ! echo "$sofout" | grep -qE 'ERROR: (AddressSanitizer|LeakSanitizer)|runtime error:'; then
            note "sanitized overflow trap" "ok (exit 134, clean trap, no sanitizer error)"
        else
            note "sanitized overflow trap" FAILED
            echo "$sofout" | sed 's/^/    /' | tail -20
            fail=1
        fi
    fi
else
    note "sanitized build" "skipped (clang cannot link a sanitized program here)"
fi

# ---- 3. the compiler side of the Part 3 contract ---------------------------
# runtime/greenthread_test.c cannot reach src/emit_c.rs at all (it is a C
# program; emit_c.rs is part of the Rust compiler). This checks the OTHER
# half of the same contract directly: that the compiler actually emits a
# call to rt_stack_check(), the real out-of-line probe rt.h documents and
# rt.c defines -- not the comparison inlined at the call site, which is
# what this used to check for before the cross-carrier caching bug was
# found and fixed (rt_stack_limit's own comment in rt.h has the account).
LANGC="./target/debug/$LANG_BIN"
if [ -x "$LANGC" ]; then
    printf 'int f() { return 1; }\n' >"$WORK/probe_fn.$LANG_EXT"
    if "$LANGC" --emit-c "$WORK/probe_fn.$LANG_EXT" -o "$WORK/probe_fn.c" 2>"$WORK/langc.err"; then
        if grep -qF 'rt_stack_check();' \
                "$WORK/probe_fn.c"; then
            note "emit_c.rs probe text" "ok (exact text found in emitted C)"
        else
            note "emit_c.rs probe text" FAILED
            echo "    expected probe text not found verbatim in emitted C"
            fail=1
        fi
        # And it must still build warning-free, exactly like every other
        # emitted program -- the probe changes every function's C, so this
        # is cheap, direct insurance beyond gates.sh's own corpus run.
        if cc -Wall -Wextra -I runtime -pthread -c "$WORK/probe_fn.c" -o "$WORK/probe_fn.o" 2>"$WORK/probe_fn.cc"; then
            if [ -s "$WORK/probe_fn.cc" ]; then
                note "emitted probe compiles clean" FAILED
                sed 's/^/    /' "$WORK/probe_fn.cc"
                fail=1
            else
                note "emitted probe compiles clean" ok
            fi
        else
            note "emitted probe compiles clean" FAILED
            sed 's/^/    /' "$WORK/probe_fn.cc" | head -20
            fail=1
        fi
    else
        note "emit_c.rs probe text" FAILED
        echo "    langc --emit-c failed: $(head -1 "$WORK/langc.err")"
        fail=1
    fi
else
    note "emit_c.rs probe text" "skipped ($LANGC not built)"
fi

exit $fail
