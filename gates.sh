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
# this per-program; this checks the runtime itself.
run "runtime compiles clean" bash -c '
    for cc in gcc clang; do
        command -v "$cc" >/dev/null || continue
        for opt in -O0 -O2; do
            out=$("$cc" "$opt" -Wall -Wextra -DRC_DEBUG -I runtime -c runtime/rt.c -o /dev/null 2>&1)
            if [ -n "$out" ]; then echo "$cc $opt:"; echo "$out"; exit 1; fi
        done
    done'

if [ $quick -eq 0 ]; then
    run "corpus" bash run.sh
fi

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall gates passed\033[0m\n'
else
    printf '\033[31m%d gate(s) FAILED\033[0m\n' "$fail"
fi
exit $fail
