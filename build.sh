#!/usr/bin/env bash
# Compile a source file to an executable. The C backend is an implementation
# detail: it emits C, hands it to cc, and cleans up after itself.
#
#   ./build.sh examples/tour.src            -> ./tour
#   ./build.sh examples/tour.src -o mybin   -> ./mybin
set -euo pipefail
cd "$(dirname "$0")"
. ./config.sh
. ./runtime/arch.sh

src=${1:?usage: ./build.sh <source> [-o out]}
out=$(basename "$src" ".$LANG_EXT")
[ "${2:-}" = "-o" ] && out=${3:?-o needs a name}

CC=${CC:-cc}
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT

"./target/debug/$LANG_BIN" --emit-c "$src" -o "$tmp/out.c"
# `spawn` and `Chan` are always green threads now (docs/concurrency-decision.md),
# so every program links the Phase 1-3 runtime unconditionally: the scheduler,
# the epoll reactor, and the x86-64 context switch they are both built on.
"$CC" -O2 -pthread -I runtime -o "$out" "$tmp/out.c" \
    runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" runtime/ctx_switch_x86_64.s
echo "$out"
