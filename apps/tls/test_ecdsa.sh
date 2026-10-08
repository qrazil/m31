#!/usr/bin/env bash
# Every check for TLS milestone M1: `lib/bignum.m31` (fixed-width bignum and
# Montgomery arithmetic) and `lib/ecdsa.m31` (ECDSA verify over P-256 and
# P-384). Same shape as `apps/ssh/test.sh`: an m31 program prints a line per
# case, a Python oracle prints the expected line, and the two are diffed.
# Nothing here compares this project's code with itself.
#
#   bash apps/tls/test_ecdsa.sh
#
# The oracles are Python's integers (`bignum_oracle.py`) and the `cryptography`
# package -- OpenSSL underneath -- (`ecdsa_oracle.py`), which generates keys
# and signatures, mutates them, and checks its own verdict on every case it
# can against the verdict the construction implies. `pip install cryptography`.
#
# Run from the repository root, with the compiler built (`cargo build`).
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh

LANGC=./target/debug/m31c
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
pass=0
fail=0

note() { printf '\033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '\033[31mFAIL\033[0m %s\n' "$1"; shift; printf '%s\n' "$@" | sed 's/^/     /'; fail=$((fail + 1)); }

build() {
    local name=$1
    if ! "$LANGC" --emit-c "apps/tls/$name.m31" -o "$WORK/$name.c" 2>"$WORK/$name.diag"; then
        bad "compile $name" "$(head -5 "$WORK/$name.diag")"
        return 1
    fi
    if ! cc -O2 -Wall -Wextra -I runtime -pthread -o "$WORK/$name" "$WORK/$name.c" \
           runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" \
           2>"$WORK/$name.cc"; then
        bad "cc $name" "$(head -5 "$WORK/$name.cc")"
        return 1
    fi
    return 0
}

# Run `$name` on `$cases`, diff its stdout with `$expected`.
compare() {
    local label=$1 name=$2 cases=$3 expected=$4
    if ! "$WORK/$name" "$cases" >"$WORK/$name.out" 2>"$WORK/$name.err"; then
        bad "$label: program failed" "$(tail -3 "$WORK/$name.err")"
        return
    fi
    if cmp -s "$WORK/$name.out" "$expected"; then
        note "$label ($(wc -l <"$expected") cases)"
    else
        bad "$label" "$(diff "$WORK/$name.out" "$expected" | head -8)"
    fi
}

# --- house style ---------------------------------------------------------------

if out=$(
    for f in apps/tls/t_bignum.m31 apps/tls/t_ecdsa_verify.m31 lib/bignum.m31 lib/ecdsa.m31; do
        "$LANGC" fmt --check "$f" || echo "$f"
    done 2>&1
) && [ -z "$out" ]; then
    note "fmt --check: the M1 sources are formatted"
else
    bad "fmt --check" "$out"
fi

# --- bignum --------------------------------------------------------------------

python3 apps/tls/bignum_oracle.py "$WORK/bignum.cases" "$WORK/bignum.expected" 2>/dev/null \
    || bad "bignum oracle failed to generate"
if build t_bignum; then
    compare "bignum: add/sub/mul/pow/inv/cmp/bit_length vs Python ints" t_bignum \
        "$WORK/bignum.cases" "$WORK/bignum.expected"
fi

# --- ecdsa ---------------------------------------------------------------------

if python3 apps/tls/ecdsa_oracle.py "$WORK/ecdsa.cases" "$WORK/ecdsa.expected" 2>"$WORK/ecdsa.gen"; then
    if build t_ecdsa_verify; then
        compare "ecdsa: P-256/P-384 verify vs OpenSSL + strict-DER rules" t_ecdsa_verify \
            "$WORK/ecdsa.cases" "$WORK/ecdsa.expected"
        sed 's/^/     /' "$WORK/t_ecdsa_verify.err"
        ones=$(grep -c ' 1$' "$WORK/ecdsa.expected")
        zeros=$(grep -c ' 0$' "$WORK/ecdsa.expected")
        if [ "$ones" -gt 50 ] && [ "$zeros" -gt 200 ]; then
            note "ecdsa corpus is not one-sided ($ones accepted, $zeros rejected)"
        else
            bad "ecdsa corpus is one-sided" "$ones accepted, $zeros rejected"
        fi
    fi
else
    bad "ecdsa oracle failed to generate" "$(tail -5 "$WORK/ecdsa.gen")"
fi

printf '\n'
if [ "$fail" -eq 0 ]; then
    printf '\033[32mall %d checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d checks failed\033[0m\n' "$fail" "$((pass + fail))"
    exit 1
fi
