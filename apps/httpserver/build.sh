#!/usr/bin/env bash
# Build apps/httpserver into ./httpserver, with the compiler in this checkout.
#
#   bash apps/httpserver/build.sh          -O2, warnings are errors
#   CC=clang bash apps/httpserver/build.sh
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./config.sh
. ./runtime/arch.sh

LANGC=${LANGC:-./target/debug/$LANG_BIN}
CC=${CC:-gcc}
OPT=${OPT:--O2}
OUT=${OUT:-apps/httpserver/httpserver}
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT

if [ ! -x "$LANGC" ]; then
    echo "compiler not built: $LANGC (run cargo build)" >&2
    exit 1
fi

"$LANGC" --emit-c "apps/httpserver/main.$LANG_EXT" -o "$W/httpserver.c" || exit 1
# The same flags run.sh holds the corpus to: the emitted C must be clean.
"$CC" "$OPT" -ffp-contract=off -Wall -Wextra -Werror -I runtime -pthread \
      -o "$OUT" "$W/httpserver.c" \
      runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" || exit 1
echo "built $OUT"
