#!/usr/bin/env bash
# Build m31ssh from a checkout of this repository.
#
#   bash apps/m31ssh/build.sh            -> ./m31ssh
#   bash apps/m31ssh/build.sh -o NAME    -> ./NAME
#
# Needs the compiler (`cargo build`) and a C compiler; the executable is the
# emitted C linked against runtime/, exactly as build.sh does for examples.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
. "$root/config.sh"
. "$root/runtime/arch.sh"
out=m31ssh
[ "${1:-}" = "-o" ] && out=${2:?-o needs a name}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
"$root/target/debug/$LANG_BIN" --emit-c "$here/m31ssh.m31" -o "$tmp/out.c"
"${CC:-cc}" -O2 -w -pthread -I "$root/runtime" -o "$out" "$tmp/out.c" \
    "$root/runtime/rt.c" "$root/runtime/scheduler.c" "$root/$RT_REACTOR_C" "$root/$RT_CTX_ASM"
echo "$out"
