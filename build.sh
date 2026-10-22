#!/usr/bin/env bash
# Compile a source file to an executable. The C backend is an implementation
# detail: it emits C, hands it to cc, and cleans up after itself.
#
#   ./build.sh examples/tour.m31            -> ./tour
#   ./build.sh examples/tour.m31 -o mybin   -> ./mybin
#   ./build.sh examples/tour.m31 --heapwatch [-o mybin]
#                                           -> a heap watch build
#                                              (docs/heap-watch-design.md)
set -euo pipefail
cd "$(dirname "$0")"
. ./config.sh
. ./runtime/arch.sh

src=${1:?usage: ./build.sh <source> [--heapwatch] [-o out]}
shift
out=$(basename "$src" ".$LANG_EXT")
hw=""
while [ $# -gt 0 ]; do
    case $1 in
        -o) out=${2:?-o needs a name}; shift 2 ;;
        --heapwatch) hw=-DRT_HEAPWATCH; shift ;;
        *) echo "usage: ./build.sh <source> [--heapwatch] [-o out]" >&2; exit 2 ;;
    esac
done

CC=${CC:-cc}
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT

"./target/debug/$LANG_BIN" -emit-c "$src" --out "$tmp/out.c"
# `spawn` and `Chan` are always green threads now (docs/concurrency-decision.md),
# so every program links the Phase 1-3 runtime unconditionally: the scheduler,
# the epoll reactor, and the context switch they are both built on
# ($RT_CTX_ASM, picked per-architecture by runtime/arch.sh above).
# Heap watch is compiled in only with --heapwatch, into the program and the
# runtime both: without it a build has none of its code (rt.h, RT_HEAPWATCH).
"$CC" -O2 -pthread $hw -I runtime -o "$out" "$tmp/out.c" \
    runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM"
echo "$out"
