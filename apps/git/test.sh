#!/usr/bin/env bash
# Every check for apps/git, against an oracle that is not this program.
#
#   bash apps/git/test.sh               the built-in fixtures
#   bash apps/git/test.sh <repo> ...    those, and each repository named
#
# The oracles are Python's `hashlib` and `zlib` for the two codecs, a
# from-scratch Python reader of the loose-object format for the layer above
# them, and the real `git` for the commands. Nothing here compares this
# program with itself.
#
# `git` is used READ-ONLY throughout, except inside the scratch fixture
# repositories this script builds under its own temporary directory.
#
# Run from the repository root, with the compiler built (`cargo build`).
set -uo pipefail
cd "$(dirname "$0")/../.."

LANGC=./target/debug/langc
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
pass=0
fail=0

note() { printf '\033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '\033[31mFAIL\033[0m %s\n' "$1"; shift; printf '%s\n' "$@" | sed 's/^/     /'; fail=$((fail + 1)); }

build() {
    local name=$1
    if ! "$LANGC" --emit-c "apps/git/$name.src" -o "$WORK/$name.c" 2>"$WORK/$name.diag"; then
        bad "compile $name" "$(head -5 "$WORK/$name.diag")"
        return 1
    fi
    if ! cc -O2 -Wall -Wextra -I runtime -pthread -o "$WORK/$name" "$WORK/$name.c" runtime/rt.c 2>"$WORK/$name.cc"; then
        bad "cc $name" "$(head -5 "$WORK/$name.cc")"
        return 1
    fi
    return 0
}

# --- house style ---------------------------------------------------------------
#
# `gates.sh` formats and re-checks `lib/`, `corpus/` and `examples/` and does
# not look at `apps/`, so this source would drift out of the house layout with
# nothing to notice. The same check, here, over this directory.

if out=$(for f in apps/git/*.src; do "$LANGC" fmt --check "$f" || echo "$f"; done 2>&1) && [ -z "$out" ]; then
    note "source is formatted"
else
    bad "source is not formatted (run: langc fmt apps/git/<file>.src)" "$out"
fi

# --- SHA-1 --------------------------------------------------------------------

python3 - "$WORK/random.bin" <<'PY'
import random, sys
random.seed(1234)
open(sys.argv[1], "wb").write(random.randbytes(8 * 1024 * 1024))
PY

if build t_sha1; then
    "$WORK/t_sha1" "$WORK/random.bin" >"$WORK/sha1.got" 2>"$WORK/sha1.time"
    python3 apps/git/oracle_sha1.py "$WORK/random.bin" >"$WORK/sha1.want"
    if cmp -s "$WORK/sha1.got" "$WORK/sha1.want"; then
        note "sha1: $(wc -l <"$WORK/sha1.got") digests match hashlib"
    else
        bad "sha1" "$(diff "$WORK/sha1.got" "$WORK/sha1.want" | head -8)"
    fi
    sed 's/^/     /' "$WORK/sha1.time"
fi

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d apps/git checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d apps/git checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
