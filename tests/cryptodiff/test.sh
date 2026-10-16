#!/usr/bin/env bash
# Random-and-edge-size differential check of the speed-sensitive primitives
# (SHA-1/256/384/512, HMAC, HKDF, ChaCha20, Poly1305, ChaCha20-Poly1305,
# AES-GCM, AES blocks) against hashlib and `cryptography`/OpenSSL. Sizes cover
# 0, 1 and every side of the 16/64/128-byte block boundaries; AEAD cases also
# round-trip and reject a flipped tag in-process.
#
#   bash tests/cryptodiff/test.sh
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

python3 tests/cryptodiff/oracle.py cases >"$WORK/cases" || { echo "FAIL oracle cases"; exit 1; }
python3 tests/cryptodiff/oracle.py want <"$WORK/cases" >"$WORK/want" || { echo "FAIL oracle (pip install cryptography)"; exit 1; }
./target/debug/m31c --emit-c tests/cryptodiff/t_diff.m31 -o "$WORK/t.c" || { echo "FAIL compile"; exit 1; }
cc -O2 -Wall -Wextra -I runtime -pthread -o "$WORK/t" "$WORK/t.c" \
    runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" 2>"$WORK/cc.err" || { echo "FAIL cc"; head "$WORK/cc.err"; exit 1; }
"$WORK/t" <"$WORK/cases" >"$WORK/got" 2>"$WORK/err"
rc=$?
if [ $rc -ne 0 ]; then
    echo "FAIL t_diff exited $rc"; tail -3 "$WORK/err"; exit 1
fi
if cmp -s "$WORK/got" "$WORK/want"; then
    printf '\033[32mok\033[0m   cryptodiff: %d cases match hashlib/cryptography\n' "$(wc -l <"$WORK/want")"
else
    echo "FAIL cryptodiff"
    paste -d'|' "$WORK/cases" "$WORK/got" "$WORK/want" | awk -F'|' '$2 != $3 {print "  case: " $1; n++} n >= 8 {exit}'
    exit 1
fi
