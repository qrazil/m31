#!/usr/bin/env bash
# Every check for TLS milestone M2: `lib/rsa.m31` (RSASSA-PKCS1-v1_5 and
# RSASSA-PSS signature verification). Same shape as `test_ecdsa.sh`: an m31
# program prints a line per case, a Python oracle prints the expected line, and
# the two are diffed. Nothing here compares this project's code with itself.
#
#   bash apps/tls/test_rsa.sh
#
# The oracle is the `cryptography` package -- OpenSSL underneath -- plus Python
# integers for the hand-built malformed encodings and the real CA roots in the
# system bundle. `pip install cryptography`.
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

# --- house style ---------------------------------------------------------------

if out=$(
    for f in apps/tls/t_rsa_verify.m31 lib/rsa.m31; do
        "$LANGC" fmt --check "$f" || echo "$f"
    done 2>&1
) && [ -z "$out" ]; then
    note "fmt --check: the M2 sources are formatted"
else
    bad "fmt --check" "$out"
fi

# --- rsa -----------------------------------------------------------------------

if python3 apps/tls/rsa_oracle.py "$WORK/rsa.cases" "$WORK/rsa.expected" 2>"$WORK/rsa.gen"; then
    sed 's/^/     /' "$WORK/rsa.gen"
    if build t_rsa_verify; then
        if "$WORK/t_rsa_verify" "$WORK/rsa.cases" >"$WORK/rsa.out" 2>"$WORK/rsa.err"; then
            if cmp -s "$WORK/rsa.out" "$WORK/rsa.expected"; then
                note "rsa: PKCS#1 v1.5 + PSS verify vs OpenSSL, forgeries, real roots ($(wc -l <"$WORK/rsa.expected") cases)"
            else
                bad "rsa" "$(diff "$WORK/rsa.out" "$WORK/rsa.expected" | head -12)"
            fi
        else
            bad "rsa: program failed" "$(tail -3 "$WORK/rsa.err")"
        fi
        sed 's/^/     /' "$WORK/rsa.err"
        ones=$(grep -c ' 1$' "$WORK/rsa.expected")
        zeros=$(grep -c ' 0$' "$WORK/rsa.expected")
        if [ "$ones" -gt 100 ] && [ "$zeros" -gt 400 ]; then
            note "rsa corpus is not one-sided ($ones accepted, $zeros rejected)"
        else
            bad "rsa corpus is one-sided" "$ones accepted, $zeros rejected"
        fi
    fi
else
    bad "rsa oracle failed to generate" "$(tail -8 "$WORK/rsa.gen")"
fi

printf '\n'
if [ "$fail" -eq 0 ]; then
    printf '\033[32mall %d checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d checks failed\033[0m\n' "$fail" "$((pass + fail))"
    exit 1
fi
