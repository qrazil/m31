#!/usr/bin/env bash
# Compile a source file to an executable. The C backend is an implementation
# detail: it emits C, hands it to cc, and cleans up after itself.
#
#   ./build.sh examples/tour.m31            -> ./tour
#   ./build.sh examples/tour.m31 -o mybin   -> ./mybin
set -euo pipefail
cd "$(dirname "$0")"
. ./config.sh

src=${1:?usage: ./build.sh <source> [-o out]}
out=$(basename "$src" ".$LANG_EXT")
[ "${2:-}" = "-o" ] && out=${3:?-o needs a name}

CC=${CC:-cc}
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT

"./target/debug/$LANG_BIN" --emit-c "$src" -o "$tmp/out.c"
"$CC" -O2 -pthread -I runtime -o "$out" "$tmp/out.c" runtime/rt.c
echo "$out"
