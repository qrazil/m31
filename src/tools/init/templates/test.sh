#!/usr/bin/env bash
# Every check for this repo.
#
#   M31_ROOT=/path/to/m31 LANGC=/path/to/m31c bash tests/test.sh
#
# What is checked:
#
#   house style    m31c -fmt --check, m31c -lint
#   unit tests     tests/t_*.m31, each prints "N passed, M failed" last
@@TEST_DOC@@
#
# Runs on bash 3.2 and BSD tools (macOS): no associative arrays, no `mapfile`.
set -uo pipefail
cd "$(dirname "$0")/.."

if [ -z "${M31_ROOT:-}" ]; then
    echo "M31_ROOT is not set -- point it at a checkout of github.com/qrazil/m31" \
         "(or an extracted release) matching the m31c version you are testing with." >&2
    exit 1
fi
if [ ! -f "$M31_ROOT/config.sh" ] || [ ! -d "$M31_ROOT/runtime" ]; then
    echo "M31_ROOT=$M31_ROOT does not look like an m31 checkout" \
         "(expected $M31_ROOT/config.sh and $M31_ROOT/runtime/)" >&2
    exit 1
fi
. "$M31_ROOT/runtime/arch.sh"

LANGC=${LANGC:-./m31c}
export LANGC M31_ROOT
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
pass=0
fail=0

note() { printf '\033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '\033[31mFAIL\033[0m %s\n' "$1"; shift; printf '%s\n' "$@" | sed 's/^/     /'; fail=$((fail + 1)); }

# build NAME: compile tests/NAME.m31 into $WORK/NAME
build() {
    local name=$1
    if ! "$LANGC" -emit-c "tests/$name.m31" --out "$WORK/$name.c" 2>"$WORK/$name.diag"; then
        bad "compile $name" "$(head -5 "$WORK/$name.diag")"
        return 1
    fi
    if ! cc -O2 -w -I "$M31_ROOT/runtime" -pthread -o "$WORK/$name" "$WORK/$name.c" \
           "$M31_ROOT/runtime/rt.c" "$M31_ROOT/runtime/scheduler.c" \
           "$M31_ROOT/$RT_REACTOR_C" "$M31_ROOT/$RT_CTX_ASM" 2>"$WORK/$name.cc"; then
        bad "cc $name" "$(head -5 "$WORK/$name.cc")"
        return 1
    fi
    return 0
}

# --- house style -----------------------------------------------------------------

if out=$("$LANGC" -fmt --check --recursive . 2>&1); then
    note "source is formatted"
else
    bad "source is not formatted (run: m31c -fmt --recursive .)" "$out"
fi

if out=$("$LANGC" -lint . 2>&1) && printf '%s\n' "$out" | grep -q '^0 finding(s)'; then
    note "lint: no findings"
else
    bad "lint" "$(printf '%s\n' "$out" | head -10)"
fi

# --- unit tests --------------------------------------------------------------------

for file in tests/t_*.m31; do
    name=$(basename "$file" .m31)
    build "$name" || continue
    "$WORK/$name" >"$WORK/$name.out" 2>&1
    last=$(tail -n 1 "$WORK/$name.out")
    failed=$(printf '%s\n' "$last" | sed -E -n 's/^([0-9]+) passed, ([0-9]+) failed.*$/\2/p')
    count=$(printf '%s\n' "$last" | sed -E -n 's/^([0-9]+) passed, ([0-9]+) failed.*$/\1/p')
    if [ -z "$count" ]; then
        bad "$name: no summary line" "$(tail -n 5 "$WORK/$name.out")"
    elif [ "$failed" = "0" ]; then
        pass=$((pass + count))
        printf '\033[32mok\033[0m   %s: %s checks\n' "$name" "$count"
    else
        pass=$((pass + count))
        fail=$((fail + failed))
        printf '\033[31mFAIL\033[0m %s: %s failed\n' "$name" "$failed"
        grep '^FAIL' "$WORK/$name.out" | head -20 | sed 's/^/     /'
    fi
done
@@TEST_APP@@
echo
if [ "$fail" -eq 0 ]; then
    printf '\033[32mall %d checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
    exit 1
fi
