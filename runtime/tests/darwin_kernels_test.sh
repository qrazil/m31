#!/usr/bin/env bash
# Informational: do the assembly kernels work on macOS?  (docs/ffi-decision.md,
# "macOS".)
#
# macOS builds run the m31 code by default (RT_HAVE_KERNELS is 0 on Darwin,
# runtime/rt.h) because the Mach-O spelling of the kernels had never run on a
# Mac and the first CI round found it broken. This builds and tests WITH them,
# through the opt-in -DRT_DARWIN_KERNELS, so a CI round says whether the
# spelling now works:
#
#   assemble   ctx_switch_<arch>.S defines every rt_k_* symbol the library declares
#   programs   the kernel corpus programs (core/1360, 1369, 1370) print what their .out says
#   cryptodiff tests/cryptodiff/test.sh against hashlib/OpenSSL with the kernels on
#   kernels    the accel feature set is not empty (the kernels are really in use)
#
# Exits 0 and says "skipped" anywhere but Darwin. The workflow step that runs
# it is continue-on-error: it must never block a release until it has been
# green once.
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./config.sh

if [ "$(uname -s)" != Darwin ]; then
    echo "skipped (not Darwin)"
    exit 0
fi
export RT_CFLAGS="-DRT_DARWIN_KERNELS"
. ./runtime/arch.sh
LANGC=${LANGC:-./target/debug/$LANG_BIN}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
fail=0
say() { printf '%-40s %s\n' "$1" "$2"; }
bad() { say "$1" FAILED; shift; printf '    %s\n' "$@"; fail=1; }

# assemble
if ! cc -c $RT_CFLAGS -I runtime "$RT_CTX_ASM" -o "$WORK/ctx.o" 2>"$WORK/as.err"; then
    bad "assemble" "$(cat "$WORK/as.err")"
else
    declared=$(cat lib/*.m31 lib/*/*.m31 | tr '\n' ' ' | grep -o 'prim "c"[^(]*__k_[A-Za-z0-9_]*' \
        | sed 's/.*__k_/_rt_k_/' | sort -u)
    defined=$(nm -g "$WORK/ctx.o" | awk '$2 == "T" {print $3}' | sort -u)
    missing=$(comm -23 <(echo "$declared") <(echo "$defined"))
    if [ -n "$missing" ]; then
        bad "assemble" "declared and not defined:" $missing
    else
        say "assemble" "ok ($(echo "$declared" | wc -l | tr -d ' ') kernels)"
    fi
fi

# programs
for t in core/1360-ffi-span-kernels core/1369-crypto-accel-detection core/1370-crypto-accel-platform; do
    if ! M31C_FFITEST=1 "$LANGC" -emit-c "corpus/$t.m31" --out "$WORK/p.c" 2>"$WORK/p.err"; then
        bad "$t" "compile failed: $(head -1 "$WORK/p.err")"
    elif ! cc -O1 -Wall -Wextra $RT_CFLAGS -I runtime -pthread -o "$WORK/p" "$WORK/p.c" \
           runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" 2>"$WORK/p.cc"; then
        bad "$t" "link failed:" "$(head -12 "$WORK/p.cc")"
    elif ! "$WORK/p" | diff - "corpus/$t.out" >"$WORK/p.diff"; then
        bad "$t" "output differs:" "$(head -6 "$WORK/p.diff")"
    else
        say "$t" ok
    fi
done

# cryptodiff, kernels on
if python3 -c "import cryptography" 2>/dev/null; then
    if bash tests/cryptodiff/test.sh >"$WORK/cd.out" 2>&1; then
        say "cryptodiff" "ok"
    else
        bad "cryptodiff" "$(tail -8 "$WORK/cd.out")"
    fi
else
    say "cryptodiff" "skipped (no python3 cryptography)"
fi
exit $fail
