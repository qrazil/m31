#!/usr/bin/env bash
# Every gate, in one place. Run after every commit.
#
# Modelled on Oro's gates, with the same rule: grep for FAILED, never for
# "ok". A gate that can only report success is not a gate.
#
#   ./gates.sh            run everything
#   ./gates.sh --quick    skip the corpus (compiler-only checks)
set -uo pipefail
cd "$(dirname "$0")"
. ./config.sh
. ./runtime/arch.sh

quick=0
[ "${1:-}" = "--quick" ] && quick=1

fail=0
run() {
    local name=$1
    shift
    printf '%-28s' "$name"
    local out
    if out=$("$@" 2>&1); then
        printf '\033[32mok\033[0m\n'
    else
        printf '\033[31mFAILED\033[0m\n'
        echo "$out" | sed 's/^/    /' | head -25
        fail=$((fail + 1))
    fi
}

run "cargo build"              cargo build
run "cargo test"               cargo test
run "cargo clippy -D warnings" cargo clippy --all-targets -- -D warnings
run "cargo fmt --check"        cargo fmt --check

# No dependencies. Not "few" -- none. Cargo.lock should name only this crate.
run "no dependencies" bash -c '
    n=$(grep -c "^name = " Cargo.lock 2>/dev/null || echo 0)
    if [ "$n" -ne 1 ]; then
        echo "Cargo.lock names $n packages; expected 1 (langc alone)"
        grep "^name = " Cargo.lock
        exit 1
    fi'

# No unsafe, in the compiler or in the runtime we ship.
run "no unsafe" bash -c '
    if grep -rn "unsafe" src/ 2>/dev/null | grep -v "^src/tests.rs" | grep -q .; then
        echo "unsafe found in src/:"
        grep -rn "unsafe" src/
        exit 1
    fi'

# The runtime must stay a separate translation unit and must not be built with
# LTO -- docs/ir-v0.md §7.1. Both would reintroduce -Wfree-nonheap-object.
run "runtime stays separate TU" bash -c '
    # Only run.sh, and only real code: this check kept matching prose about
    # itself -- first this script own source, then run.sh comment explaining
    # why LTO is off. Strip comments before looking for the flag.
    if grep -vE "^[[:space:]]*#" run.sh | grep -qE -- "-flto" 2>/dev/null; then
        echo "LTO must not be enabled: it reintroduces -Wfree-nonheap-object"
        exit 1
    fi
    if grep -qE "^static inline (void )?rc_(inc|dec)" runtime/rt.h; then
        echo "rc_inc/rc_dec must stay out-of-line in rt.c, not inline in rt.h"
        exit 1
    fi'

# Emitted C must compile warning-free under both compilers. run.sh enforces
# this per-program; this checks the runtime itself -- with each sys-layer
# backend (docs/sys-layer.md), since rt.c #includes exactly one of them and
# the other would otherwise go uncompiled. The raw backend only exists for
# Linux on the three architectures it has system call tables for.
run "runtime compiles clean" bash -c '
    backends=("")
    if [ "$(uname -s)" = Linux ]; then
        case $(uname -m) in x86_64|aarch64|riscv64) backends+=(-DRT_SYS_RAW) ;; esac
    fi
    for cc in gcc clang; do
        command -v "$cc" >/dev/null || continue
        for opt in -O0 -O2; do
            for b in "${backends[@]}"; do
                out=$("$cc" "$opt" $b -Wall -Wextra -DRC_DEBUG -I runtime -c runtime/rt.c -o /dev/null 2>&1)
                if [ -n "$out" ]; then echo "$cc $opt $b:"; echo "$out"; exit 1; fi
            done
        done
    done'

# Every function in runtime/sys.h, against every backend this machine can
# build: libc and raw natively, raw with no C library linked at all, and raw
# for aarch64 and riscv64 under qemu-user when those tools are present. Both
# backends must return the same value -- the same -errno included -- for the
# same situation, which no corpus program asks directly.
run "sys layer, every backend" bash runtime/sys_test.sh

# Phase 1 of docs/concurrency-decision.md: the x86-64 context switch, the
# slab stack allocator, the compiler-emitted probe (both sides: the runtime
# half here, and the exact text src/emit_c.rs emits), and the per-thread
# state table. Every compiler/opt combo this file already uses elsewhere,
# plus one dedicated ASan+UBSan build -- see runtime/greenthread_test.c's
# header for what is and is not reachable from a pure-C test at this phase.
run "green threads, Phase 1" bash runtime/greenthread_test.sh

# Phase 2 of docs/concurrency-decision.md: the scheduler itself (shared
# global queue with real backpressure, per-carrier local buffers with no
# stealing, self-service draw, the random-permutation wake mechanism), built
# standalone on Phase 1's primitives -- not wired to `spawn` yet, on
# purpose; see runtime/scheduler.h's header. Every compiler/opt combo this
# file already uses elsewhere, plus one dedicated ASan+UBSan build.
# ThreadSanitizer is NOT run here -- it cannot share a binary with ASan --
# see runtime/scheduler_tsan.sh, run by hand, for that.
run "scheduler, Phase 2" bash runtime/scheduler_test.sh

# Phase 3 of docs/concurrency-decision.md: epoll reactor, park/unpark (and
# the CAS state machine that closes the lost-wakeup race between a green
# thread's EAGAIN and its park call), and the blocking-FFI handoff
# (runtime/scheduler.c's additive Phase 3 surface, runtime/reactor.c/.h) --
# built on the Phase 2 scheduler, standalone, same convention. Every
# compiler/opt combo this file already uses elsewhere, plus one dedicated
# ASan+UBSan build. ThreadSanitizer is NOT run here -- see
# runtime/phase3_tsan.sh, run by hand, for that; it matters even more here
# than for Phase 2's own TSan gate (see that script's own header).
run "epoll/park-unpark/blocking-FFI, Phase 3" bash runtime/phase3_test.sh

# The formatter must not change what a program means, and must reach a fixed
# point. Both are checked against every corpus program rather than asserted:
# a formatter that quietly alters a program is worse than no formatter.
# Two checks, because neither alone is enough.
#
# The emitted C is compared with its lines SORTED, not byte-for-byte. The
# formatter deliberately reorders top-level items -- it groups methods under
# their type and hoists functions above the statements that make up the
# program -- so the definitions come out in a different order while every
# definition is identical. Sorting ignores exactly that and nothing else: a
# renamed value, a dropped field or a changed type still shows up.
#
# Order-insensitivity is a real loss, so the second check buys it back where
# it can: a core program is compiled and RUN after formatting and its output
# compared to the .out the corpus already expects. That is the property the
# byte-compare was standing in for, checked directly.
run "formatter preserves meaning" bash -c '
    bad=0
    for f in corpus/*/*.'"$LANG_EXT"' corpus/modules/*/*.'"$LANG_EXT"' examples/*.'"$LANG_EXT"'; do
        [ -e "$f" ] || continue
        w=$(mktemp -d)
        # The whole directory, not just this file: a module that imports
        # another cannot be compiled without its siblings, and the module
        # name comes from the basename so it has to keep its own.
        cp "$(dirname "$f")"/*.'"$LANG_EXT"' "$w/" 2>/dev/null
        b=$(basename "$f")
        if ./target/debug/'"$LANG_BIN"' --emit-c "$f" -o "$w/a.c" 2>/dev/null; then
            ./target/debug/'"$LANG_BIN"' fmt "$w/$b" 2>/dev/null
            ./target/debug/'"$LANG_BIN"' --emit-c "$w/$b" -o "$w/b.c" 2>/dev/null
            # String literals are numbered in the order they are first met,
            # and the formatter reorders top-level items -- so the same
            # program can emit the same literals under different numbers.
            # Blank the number before comparing; which literal each USE
            # refers to is then checked by actually running the program
            # below.
            norm() { sed -E "s/\bstr[0-9]+\b/strN/g" "$1" | sort; }
            if ! diff -q <(norm "$w/a.c") <(norm "$w/b.c") >/dev/null 2>&1; then
                echo "formatting changed the emitted C: $f"
                diff <(norm "$w/a.c") <(norm "$w/b.c") | head -6
                bad=1
            fi
            # Behaviour, not just text -- but only where an expectation exists.
            exp="${f%.'"$LANG_EXT"'}.out"
            if [ -e "$exp" ]; then
                if gcc -O0 -I runtime "$w/b.c" \
                       runtime/rt.c runtime/scheduler.c runtime/reactor.c '"$RT_CTX_ASM"' \
                       -lpthread -o "$w/b" 2>/dev/null; then
                    in="$PWD/${f%.'"$LANG_EXT"'}.in"
                    [ -e "$in" ] || in=/dev/null
                    # The same command line run.sh gives it, from `.args`.
                    argv=()
                    [ -e "${f%.'"$LANG_EXT"'}.args" ] && mapfile -t argv <"${f%.'"$LANG_EXT"'}.args"
                    # And the same fixture, from `.setup`, built in a fresh
                    # directory the program then runs in -- see run.sh.
                    rd=$PWD
                    setup="${f%.'"$LANG_EXT"'}.setup"
                    if [ -e "$setup" ]; then
                        rd="$w/run"
                        mkdir "$rd"
                        (cd "$rd" && bash "$OLDPWD/$setup") >/dev/null 2>&1
                    fi
                    if ! diff -q <(cd "$rd" && "$w/b" "${argv[@]}" 2>&1 <"$in") "$exp" >/dev/null 2>&1; then
                        echo "formatted program prints something else: $f"
                        diff <(cd "$rd" && "$w/b" "${argv[@]}" 2>&1 <"$in") "$exp" | head -6
                        bad=1
                    fi
                else
                    echo "formatted program no longer compiles: $f"
                    bad=1
                fi
            fi
        fi
        rm -rf "$w"
    done
    exit $bad'

# The standard library is source like any other, and it is the source most
# likely to be forgotten: it is not in the corpus, it is not compiled on its
# own -- `math.src` cannot be a program -- and it reaches a build through
# include_str!. Checking it is formatted is also a round-trip test, because
# the checked-in file IS the canonical output: anything the formatter drops
# or reorders in it shows up here as a diff.
run "stdlib source is formatted" bash -c '
    bad=0
    for f in lib/*.'"$LANG_EXT"'; do
        [ -e "$f" ] || continue
        ./target/debug/'"$LANG_BIN"' fmt --check "$f" || bad=1
    done
    exit $bad'

run "formatter is idempotent" bash -c '
    bad=0
    for f in corpus/*/*.'"$LANG_EXT"' corpus/modules/*/*.'"$LANG_EXT"'; do
        [ -e "$f" ] || continue
        w=$(mktemp -d)
        cp "$f" "$w/t.'"$LANG_EXT"'"
        if ./target/debug/'"$LANG_BIN"' fmt "$w/t.'"$LANG_EXT"'" 2>/dev/null; then
            cp "$w/t.'"$LANG_EXT"'" "$w/once.'"$LANG_EXT"'"
            ./target/debug/'"$LANG_BIN"' fmt "$w/t.'"$LANG_EXT"'" 2>/dev/null
            if ! diff -q "$w/once.'"$LANG_EXT"'" "$w/t.'"$LANG_EXT"'" >/dev/null 2>&1; then
                echo "formatting twice differs from once: $f"
                bad=1
            fi
        fi
        rm -rf "$w"
    done
    exit $bad'

# The layout itself. The gates above prove the formatter harmless -- same
# meaning, a fixed point -- and neither notices a comment moved to the wrong
# side of a brace or a section divider hoisted away from its section, which
# is what it did. Each `corpus/fmt/*.src` must format to its `.want`, written
# by hand, and the `.want` must already be formatted.
run "formatter fixtures" bash -c '
    bad=0
    for f in corpus/fmt/*.'"$LANG_EXT"'; do
        [ -e "$f" ] || continue
        want="${f%.'"$LANG_EXT"'}.want"
        w=$(mktemp -d)
        cp "$f" "$w/t.'"$LANG_EXT"'"
        cp "$want" "$w/w.'"$LANG_EXT"'"
        if ! ./target/debug/'"$LANG_BIN"' fmt "$w/t.'"$LANG_EXT"'"; then
            bad=1
        elif ! diff -q "$want" "$w/t.'"$LANG_EXT"'" >/dev/null; then
            echo "formatted layout differs from $want:"
            diff "$want" "$w/t.'"$LANG_EXT"'" | head -10
            bad=1
        fi
        ./target/debug/'"$LANG_BIN"' fmt --check "$w/w.'"$LANG_EXT"'" || bad=1
        rm -rf "$w"
    done
    exit $bad'

# The same input must always produce the same C.
#
# It did not: the promoted-method list for embedding was built by iterating a
# HashMap, and Rust's randomised hasher reordered the emitted definitions
# between runs of the compiler -- twelve distinct outputs from twelve runs of
# one program. Nothing caught it, because every other check compiles the C
# rather than comparing it. A build that is not a function of its input
# cannot be cached, bisected, or reproduced from a hash.
run "emission is reproducible" bash -c '
    bad=0
    for f in corpus/*/*.'"$LANG_EXT"' corpus/modules/*/*.'"$LANG_EXT"' examples/*.'"$LANG_EXT"'; do
        [ -e "$f" ] || continue
        w=$(mktemp -d)
        ./target/debug/'"$LANG_BIN"' --emit-c "$f" -o "$w/1.c" 2>/dev/null || { rm -rf "$w"; continue; }
        for _ in 1 2 3 4 5; do
            ./target/debug/'"$LANG_BIN"' --emit-c "$f" -o "$w/n.c" 2>/dev/null
            if ! diff -q "$w/1.c" "$w/n.c" >/dev/null 2>&1; then
                echo "emitted C differs between runs of the compiler: $f"
                diff "$w/1.c" "$w/n.c" | head -6
                bad=1
                break
            fi
        done
        rm -rf "$w"
    done
    exit $bad'

if [ $quick -eq 0 ]; then
    run "corpus" bash run.sh
    # The whole corpus again with the runtime on raw system calls: same
    # programs, same expected output, no C library underneath print or io.
    # Checked here on x86-64 because that is what this runs on; the raw
    # backend's other architectures are covered by the sys-layer gate above.
    if [ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ]; then
        run "corpus (raw syscalls)" env RT_CFLAGS=-DRT_SYS_RAW bash run.sh
    fi
    # Every program the corpus runs, once, under AddressSanitizer and
    # UndefinedBehaviorSanitizer. The oracle compares builds with each other,
    # so a bug that makes all four wrong the same way -- a use-after-free
    # whose freed block is reused identically -- passes it. See sanitize.sh.
    run "sanitizers (ASan, UBSan)" bash sanitize.sh
fi

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall gates passed\033[0m\n'
else
    printf '\033[31m%d gate(s) FAILED\033[0m\n' "$fail"
fi
exit $fail
