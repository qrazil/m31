#!/usr/bin/env bash
# End-to-end checks for `m31ssh` (apps/m31ssh) against a real, disposable
# OpenSSH `sshd`, driven through a pty so that interactive behaviour (raw
# mode, resize, `~.`, Ctrl-D, the host-key prompt) is exercised for real.
#
#   bash tests/ssh/test_m31ssh.sh
#
# Everything is disposable: temp host and user keys, a temp HOME for the
# client, an `sshd -D` on an ephemeral 127.0.0.1 port, a private ssh-agent.
# The real ~/.ssh is never read or written, and every process the test
# starts is killed on exit. The oracles are OpenSSH's own `sshd`,
# `ssh-keygen` (fingerprints), `ssh-agent`/`ssh-add`, and the clock.
#
# Needs sshd, ssh-keygen, ssh-agent, ssh-add and python3. Run from the
# repository root with the compiler built (`cargo build`).
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh

for tool in sshd ssh-keygen ssh-agent ssh-add python3; do
    command -v "$tool" >/dev/null || { echo "$tool not found on PATH -- skipping"; exit 0; }
done

LANGC=./target/debug/m31c
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

"$LANGC" -fmt --check --recursive apps/m31ssh || { echo "FAILED: fmt"; exit 1; }
"$LANGC" -emit-c apps/m31ssh/m31ssh.m31 --out "$WORK/m31ssh.c" 2>"$WORK/diag" || { cat "$WORK/diag"; exit 1; }
cc -O2 -w -I runtime -pthread -o "$WORK/m31ssh" "$WORK/m31ssh.c" \
    runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" || exit 1

exec python3 -I tests/ssh/test_m31ssh.py "$WORK/m31ssh" "$(command -v sshd)"
