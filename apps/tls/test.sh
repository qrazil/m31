#!/usr/bin/env bash
# Every check for the TLS 1.3 building blocks that are not the protocol
# itself: the hashes and keyed constructions the key schedule is made of
# (`lib/hmac.m31`, `lib/hkdf.m31`, and SHA-384 in `lib/sha512.m31`). No
# networking, nothing wired into `lib/http.m31`. Modelled directly on
# `apps/ssh/test.sh`, with the same rule: nothing here compares this program
# with itself.
#
#   bash apps/tls/test.sh
#
# The oracles are Python's `hashlib` and `hmac`, the `cryptography` package
# (OpenSSL underneath) for HKDF-Expand, and the published RFC 4231, RFC 5869
# and RFC 8448 values, which each oracle asserts against its own result before
# printing anything. `pip install cryptography` first.
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

# Run `t_<name>`, run `oracle_<name>.py`, and require byte-identical output.
# $2 is what the lines are, for the pass message.
check() {
    local name=$1 what=$2
    if ! build "t_$name"; then
        return
    fi
    "$WORK/t_$name" >"$WORK/$name.got" 2>"$WORK/$name.err"
    local rc=$?
    if ! python3 "apps/tls/oracle_$name.py" >"$WORK/$name.want" 2>"$WORK/$name.oracle.err"; then
        bad "$name" "oracle_$name.py failed" "$(tail -5 "$WORK/$name.oracle.err")"
    elif [ $rc -ne 0 ]; then
        bad "$name" "t_$name exited $rc" "$(tail -5 "$WORK/$name.err")"
    elif cmp -s "$WORK/$name.got" "$WORK/$name.want"; then
        note "$name: $(wc -l <"$WORK/$name.got" | tr -d ' ') lines match $what"
    else
        bad "$name" "$(diff "$WORK/$name.got" "$WORK/$name.want" | head -12)"
    fi
}

# --- house style ---------------------------------------------------------------
#
# `gates.sh` formats and re-checks `lib/` and does not look at `apps/`, so this
# source would drift out of the house layout with nothing to notice. The same
# check `apps/ssh/test.sh` runs, over this directory and the library files.

if out=$(
    for f in apps/tls/*.m31 lib/hmac.m31 lib/hkdf.m31 lib/sha512.m31; do
        "$LANGC" fmt --check "$f" || echo "$f"
    done 2>&1
) && [ -z "$out" ]; then
    note "source is formatted"
else
    bad "source is not formatted (run: m31c fmt <file>)" "$out"
fi

if ! python3 -c 'import cryptography' 2>/dev/null; then
    bad "oracles" "the 'cryptography' package is not importable -- pip install cryptography"
else
    check sha384 "hashlib (FIPS 180-4 vectors, a 0-260 length sweep, a million-a in chunks)"
    check hmac "hashlib/hmac (RFC 4231 SHA-256 and SHA-384 cases plus a message-length by key-length grid)"
    check hkdf "an independent RFC 5869/8446 implementation (RFC 5869 and RFC 8448 values, expand cross-checked against OpenSSL)"
fi

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d apps/tls checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d apps/tls checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
