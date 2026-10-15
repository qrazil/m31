#!/usr/bin/env bash
# Build one of the SSH programs from a checkout of this repository.
#
#   bash apps/build.sh m31ssh            -> ./m31ssh
#   bash apps/build.sh m31scp -o NAME    -> ./NAME
#   (m31ssh | m31scp | m31sftp | m31ssh-keygen)
#
# Needs the compiler (`cargo build`) and a C compiler; the executable is the
# emitted C linked against runtime/, exactly as build.sh does for examples.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/.." && pwd)
. "$root/config.sh"
. "$root/runtime/arch.sh"
name=${1:?usage: apps/build.sh m31ssh|m31scp|m31sftp|m31ssh-keygen [-o out]}
[ -f "$here/$name/$name.m31" ] || { echo "no such program: $name" >&2; exit 2; }
out=$name
[ "${2:-}" = "-o" ] && out=${3:?-o needs a name}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
"$root/target/debug/$LANG_BIN" --emit-c "$here/$name/$name.m31" -o "$tmp/out.c"
"${CC:-cc}" -O2 -w -pthread -I "$root/runtime" -o "$out" "$tmp/out.c" \
    "$root/runtime/rt.c" "$root/runtime/scheduler.c" "$root/$RT_REACTOR_C" "$root/$RT_CTX_ASM"
echo "$out"
