#!/usr/bin/env bash
# Every check for the Phase 1 crypto primitives (docs/ssh-decision.md):
# field25519, sha512, x25519, scalar25519, ed25519.
#
#   bash crypto25519_test/test.sh
#
# The oracle is never this project's own code checked against itself:
#
#   - field25519, scalar25519: Python's exact-precision integers, doing the
#     same modular arithmetic the field/scalar module claims to do.
#   - sha512: Python's hashlib.
#   - x25519: pynacl's libsodium binding for generic inputs; this project's
#     own from-scratch, big-integer model of RFC 7748 section 5's own
#     pseudocode (proto_x25519_bigint.py, separately checked against pynacl
#     and the RFC vectors) for the handful of low-order/non-canonical inputs
#     libsodium deliberately refuses as a safety policy on top of the bare
#     RFC function.
#   - ed25519: pynacl's libsodium binding, and RFC 8032's own section 7.1
#     test vectors (extracted programmatically from the RFC text checked
#     into this directory, never retyped by hand).
#
# Every "random" input is drawn from a SEEDED generator, not the OS's real
# randomness, so a checked-out copy of this repository regenerates the exact
# same test data and expected answers on every run -- `apps/git/test.sh`'s
# `random.seed(1234)` convention, extended to five modules.
#
# Run from the repository root, with the compiler built (`cargo build`).
set -uo pipefail
cd "$(dirname "$0")/.."

LANGC=./target/debug/langc
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
pass=0
fail=0

note() { printf '\033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '\033[31mFAIL\033[0m %s\n' "$1"; shift; printf '%s\n' "$@" | sed 's/^/     /'; fail=$((fail + 1)); }

check() {
    local name=$1
    local gen=$2
    if ! python3 "crypto25519_test/$gen" "$WORK" >"$WORK/$name.gen.log" 2>&1; then
        bad "$name: generator failed" "$(cat "$WORK/$name.gen.log")"
        return
    fi
    if ! "$LANGC" --emit-c "$WORK/t_$name.src" -o "$WORK/t_$name.c" 2>"$WORK/$name.diag"; then
        bad "$name: compile" "$(head -8 "$WORK/$name.diag")"
        return
    fi
    if ! cc -O2 -Wall -Wextra -I runtime -pthread -o "$WORK/t_$name" "$WORK/t_$name.c" runtime/rt.c 2>"$WORK/$name.cc"; then
        bad "$name: cc" "$(head -8 "$WORK/$name.cc")"
        return
    fi
    "$WORK/t_$name" >"$WORK/t_$name.got" 2>"$WORK/$name.run.err"
    if ! cmp -s "$WORK/t_$name.got" "$WORK/t_$name.want"; then
        bad "$name" "$(diff "$WORK/t_$name.got" "$WORK/t_$name.want" | head -12)" \
            "$(cat "$WORK/$name.run.err")"
        return
    fi
    note "$name: $(wc -l <"$WORK/t_$name.want") lines match the oracle"
}

check field25519 gen_field25519.py
check sha512 gen_sha512.py
check x25519 gen_x25519.py
check scalar25519 gen_scalar25519.py
check ed25519 gen_ed25519.py

# A handful of hand-picked edge cases that are awkward to fold into the
# generators above: a non-canonical y-coordinate, a y that IS canonical but
# is not on the curve at all, and an all-zero public key -- all of which
# `ed25519.verify` must refuse without crashing, per RFC 8032 5.1.3.
if "$LANGC" --emit-c crypto25519_test/t_ed25519_edge.src -o "$WORK/edge.c" 2>"$WORK/edge.diag"; then
    if cc -O2 -Wall -Wextra -I runtime -pthread -o "$WORK/edge" "$WORK/edge.c" runtime/rt.c 2>"$WORK/edge.cc"; then
        got=$("$WORK/edge")
        want=$'noncanon_y_rejected true\nnot_on_curve_rejected true\nzero_pk_no_crash true'
        if [ "$got" = "$want" ]; then
            note "ed25519 edge cases (non-canonical y, off-curve point, zero key)"
        else
            bad "ed25519 edge cases" "$got"
        fi
    else
        bad "ed25519 edge cases: cc" "$(head -8 "$WORK/edge.cc")"
    fi
else
    bad "ed25519 edge cases: compile" "$(head -8 "$WORK/edge.diag")"
fi

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall crypto25519 checks passed\033[0m (%d)\n' "$pass"
else
    printf '\033[31m%d check(s) FAILED\033[0m, %d passed\n' "$fail" "$pass"
fi
exit $fail
