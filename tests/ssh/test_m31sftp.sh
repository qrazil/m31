#!/usr/bin/env bash
# End-to-end checks for `m31scp`, `m31sftp` and `m31ssh-keygen` against a
# disposable OpenSSH `sshd` (with its internal SFTP server), cross-checked
# with the real `scp`, `sftp` and `ssh-keygen` where they exist.
#
#   bash tests/ssh/test_m31sftp.sh
#
# Everything is disposable (temp keys, temp HOME, an ephemeral port); the
# real ~/.ssh is never touched and every process is killed on exit.
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh

for tool in sshd ssh-keygen python3; do
    command -v "$tool" >/dev/null || { echo "$tool not found on PATH -- skipping"; exit 0; }
done

LANGC=./target/debug/m31c
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

build() {
    local name=$1 dir=$2
    "$LANGC" -fmt --check --recursive "apps/$dir" || { echo "FAILED: fmt $dir"; exit 1; }
    "$LANGC" -emit-c "apps/$dir/$name.m31" --out "$WORK/$name.c" 2>"$WORK/$name.diag" || { cat "$WORK/$name.diag"; exit 1; }
    cc -O2 -w -I runtime -pthread -o "$WORK/$name" "$WORK/$name.c" \
        runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" || exit 1
}
build m31scp m31scp
build m31sftp m31sftp
build m31ssh-keygen m31ssh-keygen
build m31ssh m31ssh

exec python3 -I tests/ssh/test_m31sftp.py "$WORK" "$(command -v sshd)"
