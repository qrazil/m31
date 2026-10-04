#!/usr/bin/env bash
# Every check for `docs/ssh-decision.md` Phase 1 (the crypto primitives in
# `lib/sha256.m31` and `lib/chacha20poly1305.m31`) and Phase 2's pure,
# no-`sshd` half (`lib/ssh.m31`'s wire-format functions) -- no networking,
# nothing wired into `apps/git`. Modelled directly on `apps/git/test.sh`,
# with the same rule: nothing here compares this program with itself.
# `test_transport.sh`, in this same directory, is Phase 2's other half --
# the live, disposable-`sshd` fixture that this script's pure checks cannot
# stand in for.
#
#   bash apps/ssh/test.sh
#
# The oracles are Python's `hashlib` for SHA-256, the `cryptography` package
# (OpenSSL underneath) for ChaCha20, Poly1305, the AEAD construction and
# `openssh_block`'s own word layout, and plain `hashlib` arithmetic again for
# `lib/ssh.m31`'s exchange hash and key derivation -- real, independent
# implementations, not this project's own code checked against itself.
# `pip install cryptography` first.
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
    if ! "$LANGC" --emit-c "apps/ssh/$name.m31" -o "$WORK/$name.c" 2>"$WORK/$name.diag"; then
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
#
# `gates.sh` formats and re-checks `lib/` and does not look at `apps/`, so
# this source -- and the two new `lib/` modules -- would drift out of the
# house layout with nothing to notice. The same check `apps/git/test.sh` runs
# over its own directory, here over this one plus the two library files.

if out=$(
    for f in apps/ssh/*.m31 lib/sha256.m31 lib/chacha20poly1305.m31 lib/ssh.m31; do
        "$LANGC" fmt --check "$f" || echo "$f"
    done 2>&1
) && [ -z "$out" ]; then
    note "source is formatted"
else
    bad "source is not formatted (run: m31c fmt <file>)" "$out"
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

# --- openssh_block: the second ChaCha20 word layout -----------------------------

if ! python3 -c 'import cryptography' 2>/dev/null; then
    bad "chacha20_openssh" "the 'cryptography' package is not importable -- pip install cryptography"
else
    if build t_chacha20_openssh; then
        "$WORK/t_chacha20_openssh" >"$WORK/co.got" 2>"$WORK/co.err"
        rc=$?
        python3 apps/ssh/oracle_chacha20_openssh.py >"$WORK/co.want"
        if [ $rc -ne 0 ]; then
            bad "chacha20_openssh" "t_chacha20_openssh exited $rc" "$(tail -5 "$WORK/co.err")"
        elif cmp -s "$WORK/co.got" "$WORK/co.want"; then
            note "chacha20_openssh: $(wc -l <"$WORK/co.got") lines match cryptography/OpenSSL (fixed edge cases plus a 64-case sweep)"
        else
            bad "chacha20_openssh" "$(diff "$WORK/co.got" "$WORK/co.want" | head -12)"
        fi
        sed 's/^/     /' "$WORK/co.err"
    fi
fi

# --- lib/ssh.m31 Phase 2: wire-format self-checks (no oracle needed) ------------

if build t_ssh_proto; then
    out=$("$WORK/t_ssh_proto" 2>"$WORK/proto.err")
    rc=$?
    if [ $rc -eq 0 ] && [[ "$out" == *"0 failures"* ]]; then
        note "ssh wire-format self-checks: $out"
    else
        bad "ssh wire-format self-checks" "exited $rc" "$out" "$(tail -10 "$WORK/proto.err")"
    fi
fi

# --- lib/ssh.m31 Phase 2: exchange hash and key derivation ----------------------

if build t_ssh_wire; then
    "$WORK/t_ssh_wire" >"$WORK/wire.got" 2>"$WORK/wire.err"
    rc=$?
    python3 apps/ssh/oracle_ssh_wire.py >"$WORK/wire.want"
    if [ $rc -ne 0 ]; then
        bad "ssh wire (exchange hash / key derivation)" "t_ssh_wire exited $rc" "$(tail -5 "$WORK/wire.err")"
    elif cmp -s "$WORK/wire.got" "$WORK/wire.want"; then
        note "ssh wire (exchange hash / key derivation): $(wc -l <"$WORK/wire.got") lines match independent hashlib math"
    else
        bad "ssh wire (exchange hash / key derivation)" "$(diff "$WORK/wire.got" "$WORK/wire.want" | head -12)"
    fi
    sed 's/^/     /' "$WORK/wire.err"
fi

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d apps/ssh checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d apps/ssh checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
