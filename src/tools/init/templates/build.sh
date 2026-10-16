#!/usr/bin/env bash
# Build @@NAME@@ into a real executable.
#
#   M31_ROOT=/path/to/m31 LANGC=/path/to/m31c bash scripts/build.sh
#       -> ./@@NAME@@
#   M31_ROOT=/path/to/m31 LANGC=/path/to/m31c bash scripts/build.sh -o name
#       -> ./name
#
# m31c only emits C; the executable is that C linked against the m31 runtime's
# own source files, so this needs both:
#
#   - LANGC: the m31c compiler binary. Default ./m31c.
#   - M31_ROOT: a directory containing config.sh and runtime/ (a checkout of
#     github.com/qrazil/m31, or an extracted release) matching LANGC's version.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ -z "${M31_ROOT:-}" ]; then
    echo "M31_ROOT is not set -- point it at a checkout of github.com/qrazil/m31" \
         "(or an extracted release) matching the m31c version you are building with." >&2
    exit 1
fi
if [ ! -f "$M31_ROOT/config.sh" ] || [ ! -d "$M31_ROOT/runtime" ]; then
    echo "M31_ROOT=$M31_ROOT does not look like an m31 checkout" \
         "(expected $M31_ROOT/config.sh and $M31_ROOT/runtime/)" >&2
    exit 1
fi
. "$M31_ROOT/config.sh"
. "$M31_ROOT/runtime/arch.sh"

out=@@NAME@@
[ "${1:-}" = "-o" ] && out=${2:?-o needs a name}

LANGC=${LANGC:-./m31c}
CC=${CC:-cc}
if [ ! -x "$LANGC" ]; then
    echo "compiler not found or not executable: $LANGC" >&2
    exit 1
fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

"$LANGC" -emit-c @@IDENT@@.m31 --out "$tmp/out.c"
# RT_REACTOR_C and RT_CTX_ASM (from runtime/arch.sh) are relative to M31_ROOT.
"$CC" -O2 -pthread -I "$M31_ROOT/runtime" -o "$out" "$tmp/out.c" \
    "$M31_ROOT/runtime/rt.c" "$M31_ROOT/runtime/scheduler.c" \
    "$M31_ROOT/$RT_REACTOR_C" "$M31_ROOT/$RT_CTX_ASM"
echo "$out"
