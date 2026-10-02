#!/usr/bin/env bash
# Every check for `docs/ssh-decision.md` Phase 1: the crypto primitives in
# `lib/sha256.src` and `lib/chacha20poly1305.src`, standalone -- no
# networking, nothing wired into `apps/git`. Modelled directly on
# `apps/git/test.sh`, with the same rule: nothing here compares this
# program with itself.
#
#   bash apps/ssh/test.sh
#
# The oracles are Python's `hashlib` for SHA-256 and the `cryptography`
# package (OpenSSL underneath) for ChaCha20, Poly1305 and the AEAD
# construction -- real, independent implementations, not this project's own
# code checked against itself. `pip install cryptography` first.
#
# Run from the repository root, with the compiler built (`cargo build`).
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh

LANGC=./target/debug/langc
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
pass=0
fail=0

note() { printf '\033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '\033[31mFAIL\033[0m %s\n' "$1"; shift; printf '%s\n' "$@" | sed 's/^/     /'; fail=$((fail + 1)); }

build() {
    local name=$1
    if ! "$LANGC" --emit-c "apps/ssh/$name.src" -o "$WORK/$name.c" 2>"$WORK/$name.diag"; then
        bad "compile $name" "$(head -5 "$WORK/$name.diag")"
        return 1
    fi
    if ! cc -O2 -Wall -Wextra -I runtime -pthread -o "$WORK/$name" "$WORK/$name.c" \
           runtime/rt.c runtime/scheduler.c runtime/reactor.c "$RT_CTX_ASM" \
           2>"$WORK/$name.cc"; then
        bad "cc $name" "$(head -5 "$WORK/$name.cc")"
        return 1
    fi
    return 0
}

# --- house style ---------------------------------------------------------------
#
# `gates.sh` formats and re-checks `lib/` and does not look at `apps/`, so
# this source -- and the two new `lib/` modules -- would drift out of the
# house layout with nothing to notice. The same check `apps/git/test.sh` runs
# over its own directory, here over this one plus the two library files.

if out=$(
    for f in apps/ssh/*.src lib/sha256.src lib/chacha20poly1305.src; do
        "$LANGC" fmt --check "$f" || echo "$f"
    done 2>&1
) && [ -z "$out" ]; then
    note "source is formatted"
else
    bad "source is not formatted (run: langc fmt <file>)" "$out"
fi

# --- SHA-256 --------------------------------------------------------------------

if build t_sha256; then
    "$WORK/t_sha256" >"$WORK/sha256.got" 2>"$WORK/sha256.err"
    python3 apps/ssh/oracle_sha256.py >"$WORK/sha256.want"
    if cmp -s "$WORK/sha256.got" "$WORK/sha256.want"; then
        note "sha256: $(wc -l <"$WORK/sha256.got") digests match hashlib"
    else
        bad "sha256" "$(diff "$WORK/sha256.got" "$WORK/sha256.want" | head -8)"
    fi
    sed 's/^/     /' "$WORK/sha256.err"
fi

# --- ChaCha20, Poly1305, and the AEAD construction ------------------------------

if ! python3 -c 'import cryptography' 2>/dev/null; then
    bad "chacha20poly1305" "the 'cryptography' package is not importable -- pip install cryptography"
else
    if build t_chacha20poly1305; then
        "$WORK/t_chacha20poly1305" >"$WORK/ccp.got" 2>"$WORK/ccp.err"
        rc=$?
        python3 apps/ssh/oracle_chacha20poly1305.py >"$WORK/ccp.want"
        if [ $rc -ne 0 ]; then
            bad "chacha20poly1305" "t_chacha20poly1305 exited $rc -- a self-check (round trip or tamper detection) trapped" \
                "$(tail -5 "$WORK/ccp.err")"
        elif cmp -s "$WORK/ccp.got" "$WORK/ccp.want"; then
            note "chacha20poly1305: $(wc -l <"$WORK/ccp.got") lines match cryptography/OpenSSL (RFC 8439 vectors, a 0-260 length sweep, a 19x19 AAD/plaintext length grid with in-process round-trip and tamper checks, and 900 further random-ish cases)"
        else
            bad "chacha20poly1305" "$(diff "$WORK/ccp.got" "$WORK/ccp.want" | head -12)"
        fi
        sed 's/^/     /' "$WORK/ccp.err"
    fi
fi

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d apps/ssh checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d apps/ssh checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
