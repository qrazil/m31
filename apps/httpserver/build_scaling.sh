#!/usr/bin/env bash
# Build apps/httpserver into ./httpserver_scaling -- identical to build.sh's
# ./httpserver EXCEPT it also links greenthread_probe.c (a SIGUSR1-based,
# read-only introspection probe, see that file's own header comment) for
# use by SCALING.md's ramping concurrency test. The shipped build.sh/
# ./httpserver binary used for BENCHMARK.md is untouched by this script.
#
#   bash apps/httpserver/build_scaling.sh
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./config.sh
. ./runtime/arch.sh

LANGC=${LANGC:-./target/debug/$LANG_BIN}
CC=${CC:-gcc}
OPT=${OPT:--O2}
OUT=${OUT:-apps/httpserver/httpserver_scaling}
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT

if [ ! -x "$LANGC" ]; then
    echo "compiler not built: $LANGC (run cargo build)" >&2
    exit 1
fi

"$LANGC" --emit-c "apps/httpserver/main.$LANG_EXT" -o "$W/httpserver.c" || exit 1
"$CC" "$OPT" -ffp-contract=off -Wall -Wextra -Werror -I runtime -pthread \
      -o "$OUT" "$W/httpserver.c" apps/httpserver/greenthread_probe.c \
      runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" || exit 1
echo "built $OUT"
