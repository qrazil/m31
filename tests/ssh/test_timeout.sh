#!/usr/bin/env bash
# Read timeouts on the ssh transport and exec channel, against a real `sshd`.
#
# `ssh.connect_any(..., timeout_ms:)`, `sshclient.connect(..., timeout_ms:)`
# and `Session.set_read_timeout` bound every wait for the server. The oracle
# is the clock and a real OpenSSH `sshd` (a disposable one, as in
# `test_auth_exec.sh`): a command that sleeps on the real server must make
# the m31 client fail with `TimedOut` after about the timeout and not after
# the command; a peer that accepts and says nothing (Python's `socket`)
# must fail the handshake the same way; and a slow-but-answering command
# whose total time exceeds the timeout, but never one wait, must succeed.
#
#   bash tests/ssh/test_timeout.sh
#
# Needs `sshd`, `ssh-keygen` and `python3`. Run from the repository root,
# with the compiler built (`cargo build`).
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh

for tool in sshd ssh-keygen python3; do
    command -v "$tool" >/dev/null || { echo "$tool not found on PATH -- skipping"; exit 0; }
done
SSHD_BIN=$(command -v sshd)

LANGC=./target/debug/m31c
WORK=$(mktemp -d)
server_pid=""
silent_pid=""
pass=0
fail=0
cleanup() {
    [ -n "$server_pid" ] && kill "$server_pid" >/dev/null 2>&1
    [ -n "$silent_pid" ] && kill "$silent_pid" >/dev/null 2>&1
    wait 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT
note() { printf '\033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '\033[31mFAIL\033[0m %s\n' "$1"; shift; printf '%s\n' "$@" | sed 's/^/     /'; fail=$((fail + 1)); }

"$LANGC" fmt --check tests/ssh/t_ssh_timeout.m31 || { echo "FAILED: fmt"; exit 1; }
"$LANGC" --emit-c tests/ssh/t_ssh_timeout.m31 -o "$WORK/t.c" || exit 1
cc -O2 -I runtime -pthread -o "$WORK/t" "$WORK/t.c" \
    runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" || exit 1

ssh-keygen -t ed25519 -f "$WORK/host_key" -N "" -q
ssh-keygen -t ed25519 -f "$WORK/user_key" -N "" -q
chmod 600 "$WORK/host_key" "$WORK/user_key"
cp "$WORK/user_key.pub" "$WORK/authorized_keys"
USER_NAME=$(id -un)
PORT=$((20000 + (RANDOM % 20000)))
SILENT=$((PORT + 1))
printf '[127.0.0.1]:%s %s %s\n' "$PORT" $(cut -d' ' -f1,2 "$WORK/host_key.pub") >"$WORK/kh"
# Any file will do for the silent peer: the key is loaded first, the
# known_hosts file is read, and then nothing ever answers the handshake.
printf '[127.0.0.1]:%s %s %s\n' "$SILENT" $(cut -d' ' -f1,2 "$WORK/host_key.pub") >>"$WORK/kh"

cat >"$WORK/sshd_config" <<EOF
Port $PORT
ListenAddress 127.0.0.1
HostKey $WORK/host_key
PidFile $WORK/sshd.pid
AuthorizedKeysFile $WORK/authorized_keys
StrictModes no
UsePAM no
KexAlgorithms curve25519-sha256
HostKeyAlgorithms ssh-ed25519
Ciphers chacha20-poly1305@openssh.com
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
EOF
"$SSHD_BIN" -f "$WORK/sshd_config" -D -e >"$WORK/sshd.log" 2>&1 &
server_pid=$!

python3 -I - "$SILENT" <<'PY' &
import socket, sys, time
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", int(sys.argv[1])))
s.listen(8)
held = []
while True:
    c, _ = s.accept()
    held.append(c)          # accept, then say nothing at all
PY
silent_pid=$!

for p in "$PORT" "$SILENT"; do
    up=0
    for _ in $(seq 1 100); do
        if (exec 3<>"/dev/tcp/127.0.0.1/$p") 2>/dev/null; then
            exec 3>&-
            up=1
            break
        fi
        sleep 0.05
    done
    [ "$up" = 1 ] || { bad "nothing listening on $p" "$(cat "$WORK/sshd.log")"; exit 1; }
done

# run MODE PORT -> prints "<word> <elapsed ms>"
run() {
    local mode=$1 port=$2 t0 t1 out
    t0=$(date +%s%N)
    out=$(timeout 60 "$WORK/t" "$mode" "$port" "$USER_NAME" "$WORK/user_key" "$WORK/kh" 2>"$WORK/err")
    t1=$(date +%s%N)
    printf '%s %s' "$out" $(((t1 - t0) / 1000000))
}

check() { # label, mode, port, expected word, min ms, max ms
    local got word ms
    got=$(run "$2" "$3")
    word=${got% *}
    ms=${got##* }
    if [ "$word" = "$4" ] && [ "$ms" -ge "$5" ] && [ "$ms" -le "$6" ]; then
        note "$1: '$word' after ${ms} ms"
    else
        bad "$1" "want '$4' in $5..$6 ms, got '$word' after ${ms} ms" "$(head -3 "$WORK/err")" "sshd: $(tail -3 "$WORK/sshd.log")"
    fi
}

check "silent peer, handshake timeout 600 ms"    connect-silent "$SILENT" connect-timed-out 500 4000
check "sleep 8 on sshd, connect timeout 800 ms"  exec-stall    "$PORT"   read-timed-out    700 5000
check "sleep 8 on sshd, set_read_timeout 800 ms" exec-after    "$PORT"   read-timed-out    700 5000
check "echo hi, timeout armed"                   exec-fast     "$PORT"   hi                0   3000
check "sleep 1; echo done, timeout 2000 ms"      exec-slow-ok  "$PORT"   done              900 5000

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d tests/ssh/test_timeout.sh checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d tests/ssh/test_timeout.sh checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
