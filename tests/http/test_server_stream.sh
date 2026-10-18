#!/usr/bin/env bash
# `http.read_request_stream` and `http.start_response` on a live socket.
#
# Python sends a 48 MiB body -- half again `MAX_BODY`, so `read_request`
# would refuse it -- with a Content-Length, and reads the answer on a second
# thread while it is still sending. The server (tests/http/t_server_stream.m31)
# echoes the body back chunked, a piece at a time. Passing needs all three:
# the echo is byte-for-byte what was sent; the server relayed every byte;
# and, where /proc exists, its peak resident memory stayed under 32 MiB,
# which a server that held the body could not.
#
#   bash tests/http/test_server_stream.sh
#
# Run from the repository root, with the compiler built (`cargo build`).
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh

LANGC=./target/debug/m31c
WORK=$(mktemp -d)
srv_pid=""
cleanup() {
    [ -n "$srv_pid" ] && kill "$srv_pid" 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

"$LANGC" -fmt --check tests/http/t_server_stream.m31 || { echo "FAILED: fmt"; exit 1; }
"$LANGC" -emit-c tests/http/t_server_stream.m31 --out "$WORK/s.c" || exit 1
cc -O2 -I runtime -pthread -o "$WORK/s" "$WORK/s.c" \
    runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" || exit 1

"$WORK/s" >"$WORK/out" 2>"$WORK/err" &
srv_pid=$!
for _ in $(seq 1 100); do
    [ -s "$WORK/err" ] && break
    sleep 0.1
done
port=$(head -1 "$WORK/err")
[ -n "$port" ] || { echo "FAILED: server printed no port"; cat "$WORK/err"; exit 1; }

python3 -I - "$port" <<'PY'
import hashlib, socket, sys, threading

port = int(sys.argv[1])
size = 48 * 1024 * 1024
block = hashlib.sha256(b"seed").digest() * 2048           # 64 KiB
sent = hashlib.sha256()
got = bytearray()


def receive(s):
    while True:
        piece = s.recv(1 << 16)
        if not piece:
            return
        got.extend(piece)


s = socket.create_connection(("127.0.0.1", port), timeout=60)
reader = threading.Thread(target=receive, args=(s,))
reader.start()
s.sendall(b"PUT /up HTTP/1.1\r\nHost: h\r\nContent-Length: %d\r\n\r\n" % size)
left = size
while left:
    piece = block[:min(left, len(block))]
    s.sendall(piece)
    sent.update(piece)
    left -= len(piece)
reader.join(60)
s.close()

data = bytes(got)
at = data.index(b"\r\n\r\n") + 4
head = data[:at]
body = bytearray()
while True:
    eol = data.index(b"\r\n", at)
    n = int(data[at:eol].split(b";")[0], 16)
    if n == 0:
        break
    body.extend(data[eol + 2:eol + 2 + n])
    at = eol + 2 + n + 2
ok = head.startswith(b"HTTP/1.1 200") and b"transfer-encoding: chunked" in head.lower()
ok = ok and len(body) == size and hashlib.sha256(body).digest() == sent.digest()
print(("ok   " if ok else "FAIL ") + "echo of %d bytes (got %d)" % (size, len(body)))
sys.exit(0 if ok else 1)
PY
py=$?

wait "$srv_pid"
rc=$?
srv_pid=""
relayed=$(grep '^relayed ' "$WORK/out" | cut -d' ' -f2)
peak=$(grep '^peak ' "$WORK/out" | cut -d' ' -f2)
if [ $py -ne 0 ] || [ $rc -ne 0 ] || [ "$relayed" != "50331648" ]; then
    echo "FAILED: client $py, server exit $rc"
    cat "$WORK/out"
    exit 1
fi
if [ "${peak:--1}" -ge 32768 ]; then
    echo "FAILED: peak resident memory ${peak} KiB, the body was held"
    exit 1
fi
echo "passed: 48 MiB echoed, peak ${peak} KiB"
