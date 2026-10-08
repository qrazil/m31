#!/usr/bin/env bash
# Every check for AES and AES-GCM (`lib/aes.m31`, `lib/aesgcm.m31`): the block
# cipher against FIPS 197 and OpenSSL, GCM against the McGrew & Viega
# specification's published cases and OpenSSL, the negative tests (every
# corruption of a sealed message is refused) and the caller-bug traps. No
# networking. The TLS suites that use them are checked by `test_tls_suites.sh`.
#
#   bash apps/tls/test_aesgcm.sh
#
# The oracle is the `cryptography` package (OpenSSL underneath); each oracle
# asserts the published values against its own result before printing
# anything. `pip install cryptography` first. Run from the repository root,
# with the compiler built (`cargo build`).
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

# Run harness $1, run oracle $2, and require byte-identical output.
# $3 is what the lines are, for the pass message.
check() {
    local name=$1 oracle=$2 what=$3
    if ! build "$name"; then
        return
    fi
    "$WORK/$name" >"$WORK/$name.got" 2>"$WORK/$name.err"
    local rc=$?
    if ! python3 "apps/tls/$oracle" >"$WORK/$name.want" 2>"$WORK/$name.oracle.err"; then
        bad "$name" "$oracle failed" "$(tail -5 "$WORK/$name.oracle.err")"
    elif [ $rc -ne 0 ]; then
        bad "$name" "$name exited $rc" "$(tail -5 "$WORK/$name.err")"
    elif cmp -s "$WORK/$name.got" "$WORK/$name.want"; then
        note "$name: $(wc -l <"$WORK/$name.got" | tr -d ' ') lines match $what"
    else
        bad "$name" "$(diff "$WORK/$name.got" "$WORK/$name.want" | head -12)"
    fi
}

# --- house style ---------------------------------------------------------------

if out=$(
    for f in apps/tls/aes_*.m31 apps/tls/gcm_*.m31 lib/aes.m31 lib/aesgcm.m31; do
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
    check aes_block aes_block_oracle.py "OpenSSL AES (FIPS 197 appendix B/C values, 2 key sizes x 40 keys, zero and ones, every byte value)"
    check gcm_seal gcm_oracle.py "OpenSSL AES-GCM (the GCM specification's cases 1-4 and 13-16, a length sweep, full records, and every tamper refused)"
fi

# --- caller bugs trap, with a diagnostic naming the module ---------------------

if build gcm_traps; then
    if out=$("$WORK/gcm_traps" ok 2>&1) && [ "$out" = "sealed 21" ]; then
        note "gcm_traps: the control call succeeds"
    else
        bad "gcm_traps: control" "$out"
    fi
    for mode in key-15 key-24 key-0 nonce-11 nonce-13 open-key-17 open-nonce-8 aes-key-8 aes-block-15 aes-block-17; do
        out=$("$WORK/gcm_traps" "$mode" 2>&1)
        rc=$?
        case "$out" in
            *"trap: aes"*) want=yes ;;
            *) want=no ;;
        esac
        if [ $rc -ne 0 ] && [ $want = yes ]; then
            note "gcm_traps $mode: traps ($(printf '%s' "$out" | head -1))"
        else
            bad "gcm_traps $mode" "exit $rc" "$out"
        fi
    done
fi

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d aes/gcm checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d aes/gcm checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
