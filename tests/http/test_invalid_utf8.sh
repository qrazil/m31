#!/usr/bin/env bash
# `http.read_request` and bytes that are not UTF-8.
#
# A server must answer, or refuse, a request whose request line, headers or
# body hold invalid UTF-8; it must never trap (a trap kills every connection
# the process holds). Fourteen connections are made to a server built from
# tests/http/t_server_utf8.m31 by Python's `socket`, which is the oracle on
# two points that are not this program's own: what is on the wire (raw
# bytes, written by Python, not by this module) and the outcome RFC 9112
# requires (a status line, 4xx for a malformed head, 2xx where the bytes are
# opaque -- a message body). The final connection is a plain valid request,
# which only an alive server can answer, and the server's own last line is
# `served 14` with exit status 0.
#
#   bash tests/http/test_invalid_utf8.sh
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

"$LANGC" --emit-c tests/http/t_server_utf8.m31 -o "$WORK/s.c" || exit 1
cc -O2 -I runtime -pthread -o "$WORK/s" "$WORK/s.c" \
    runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" || exit 1
"$LANGC" fmt --check tests/http/t_server_utf8.m31 || { echo "FAILED: fmt"; exit 1; }

"$WORK/s" >"$WORK/out" 2>"$WORK/err" &
srv_pid=$!
for _ in $(seq 1 100); do
    [ -s "$WORK/err" ] && break
    sleep 0.1
done
port=$(head -1 "$WORK/err")
[ -n "$port" ] || { echo "FAILED: server printed no port"; cat "$WORK/err"; exit 1; }

python3 -I - "$port" <<'PY'
import socket, sys

port = int(sys.argv[1])
H = b"Host: h\r\n"
# (label, raw bytes, allowed status codes)
cases = [
    ("target-ff",        b"GET /\xff HTTP/1.1\r\n" + H + b"\r\n", {400}),
    ("target-c3-paren",  b"GET /\xc3( HTTP/1.1\r\n" + H + b"\r\n", {400}),
    ("absolute-bad",     b"GET http\xff//h/x HTTP/1.1\r\n" + H + b"\r\n", {400}),
    ("header-value-ff",  b"GET /a HTTP/1.1\r\n" + H + b"X-A: \xff\xfe\r\n\r\n", {200, 400}),
    ("header-name-ff",   b"GET /a HTTP/1.1\r\n" + H + b"X-\xffA: v\r\n\r\n", {400}),
    ("host-ff",          b"GET /a HTTP/1.1\r\nHost: \xffh\r\n\r\n", {400}),
    ("method-ff",        b"G\xffT /a HTTP/1.1\r\n" + H + b"\r\n", {400}),
    ("version-ff",       b"GET /a HTTP/1.\xff\r\n" + H + b"\r\n", {400}),
    ("body-ff-cl",       b"POST /b HTTP/1.1\r\n" + H + b"Content-Length: 4\r\n\r\n\xff\xfe\xc3(", {200}),
    ("body-ff-chunked",  b"POST /c HTTP/1.1\r\n" + H + b"Transfer-Encoding: chunked\r\n\r\n4\r\n\xff\xfe\xc3(\r\n0\r\n\r\n", {200}),
    ("pct-ff",           b"GET /%ff HTTP/1.1\r\n" + H + b"\r\n", {400}),
    ("target-utf8-ok",   "GET /é HTTP/1.1\r\n".encode() + H + b"\r\n", {200}),
    ("query-ff",         b"GET /q?x=\xff HTTP/1.1\r\n" + H + b"\r\n", {200, 400}),
    ("final-valid",      b"GET /fin HTTP/1.1\r\n" + H + b"Connection: close\r\n\r\n", {200}),
]
failed = 0
for label, raw, ok in cases:
    s = socket.create_connection(("127.0.0.1", port), timeout=10)
    s.sendall(raw)
    try:
        s.shutdown(socket.SHUT_WR)
    except OSError:
        pass
    data = b""
    try:
        while True:
            chunk = s.recv(65536)
            if not chunk:
                break
            data += chunk
    except OSError as e:
        data += b""
    s.close()
    line = data.split(b"\r\n", 1)[0]
    parts = line.split(b" ")
    status = int(parts[1]) if len(parts) > 1 and parts[0].startswith(b"HTTP/") and parts[1].isdigit() else None
    good = status in ok
    if label == "final-valid":
        good = good and data.endswith(b"ok /fin")
    print(("ok   " if good else "FAIL ") + label + " -> " + repr(line))
    failed += 0 if good else 1
sys.exit(1 if failed else 0)
PY
py=$?

wait "$srv_pid"
rc=$?
srv_pid=""
last=$(tail -1 "$WORK/out")
if [ $py -ne 0 ]; then
    echo "FAILED: client checks"
    exit 1
fi
if [ $rc -ne 0 ] || [ "$last" != "served 14" ]; then
    echo "FAILED: server exit $rc, last line '$last'"
    cat "$WORK/err"
    exit 1
fi
echo "passed: 14 requests, server alive, exit 0"
