#!/usr/bin/env bash
# Build apps/markdown into ./markdown, with the compiler in this checkout.
#
#   bash apps/markdown/build.sh          -O2, warnings are errors
#   CC=clang bash apps/markdown/build.sh
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./config.sh
. ./runtime/arch.sh

LANGC=${LANGC:-./target/debug/$LANG_BIN}
CC=${CC:-gcc}
OPT=${OPT:--O2}
OUT=${OUT:-apps/markdown/markdown}
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT

if [ ! -x "$LANGC" ]; then
    echo "compiler not built: $LANGC (run cargo build)" >&2
    exit 1
fi

"$LANGC" --emit-c "apps/markdown/main.$LANG_EXT" -o "$W/markdown.c" || exit 1
# The same flags run.sh holds the corpus to: the emitted C must be clean.
"$CC" "$OPT" -ffp-contract=off -Wall -Wextra -Werror -I runtime -pthread \
      -o "$OUT" "$W/markdown.c" \
      runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" runtime/ctx_switch_x86_64.s || exit 1
echo "built $OUT"
