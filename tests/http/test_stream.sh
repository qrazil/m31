#!/usr/bin/env bash
# `http.fetch_stream` / `BodyStream.read` against a Python origin server.
#
# The server (Python's `http.server`, written below) sends each body in two
# parts and does not send the second until the client has created a marker
# file after its FIRST read -- so a client that buffered the body before
# returning would deadlock, and one that streams passes. Three framings are
# covered (Content-Length, chunked with an extension and a trailer, and
# close-delimited) plus a body that stops short of its Content-Length, which
# must be an error and not a quiet short body. The body the client wrote to
# disk is compared byte for byte with what Python sent.
#
#   bash tests/http/test_stream.sh
#
# Run from the repository root, with the compiler built (`cargo build`).
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh

LANGC=./target/debug/m31c
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

"$LANGC" -fmt --check tests/http/t_stream_client.m31 || { echo "FAILED: fmt"; exit 1; }
"$LANGC" -emit-c tests/http/t_stream_client.m31 --out "$WORK/c.c" || exit 1
cc -O2 -I runtime -pthread -o "$WORK/client" "$WORK/c.c" \
    runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" || exit 1

python3 -I - "$WORK" <<'PY'
import hashlib, http.server, os, subprocess, sys, threading, time

work = sys.argv[1]
body = hashlib.sha256(b"seed").digest() * 4000          # 128000 bytes
first, rest = body[:10000], body[10000:]
marker = os.path.join(work, "marker")
short = body[:100]


def wait_marker():
    end = time.time() + 10
    while time.time() < end:
        if os.path.exists(marker):
            return True
        time.sleep(0.02)
    return False


class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_GET(self):
        w = self.wfile
        if self.path == "/length":
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            w.write(first); w.flush()
            if wait_marker():
                w.write(rest)
        elif self.path == "/chunked":
            self.send_response(200)
            self.send_header("Transfer-Encoding", "chunked")
            self.send_header("Connection", "close")
            self.end_headers()

            def chunk(b, ext=b""):
                w.write(b"%x%s\r\n" % (len(b), ext) + b + b"\r\n")
            chunk(first[:7000], b";note=1"); chunk(first[7000:]); w.flush()
            if wait_marker():
                for i in range(0, len(rest), 30000):
                    chunk(rest[i:i + 30000])
                w.write(b"0\r\nX-Trailer: yes\r\n\r\n")
        elif self.path == "/close":
            self.send_response(200)
            self.send_header("Connection", "close")
            self.end_headers()
            w.write(first); w.flush()
            if wait_marker():
                w.write(rest)
            self.close_connection = True
        elif self.path == "/short":
            self.send_response(200)
            self.send_header("Content-Length", "5000")
            self.send_header("Connection", "close")
            self.end_headers()
            w.write(short); w.flush()
        self.close_connection = True


srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
port = srv.server_address[1]
threading.Thread(target=srv.serve_forever, daemon=True).start()

failed = 0


def run(path, expect_ok):
    global failed
    out = os.path.join(work, "out" + path.replace("/", "_"))
    if os.path.exists(marker):
        os.remove(marker)
    r = subprocess.run([os.path.join(work, "client"),
                        "http://127.0.0.1:%d%s" % (port, path), out, marker],
                       capture_output=True, text=True, timeout=60)
    got = open(out, "rb").read() if os.path.exists(out) else b""
    lines = r.stdout.split("\n")
    if expect_ok:
        good = (r.returncode == 0 and "status 200" in lines and got == body
                and "reads>1 true" in lines and ("bytes %d" % len(body)) in lines)
    else:
        good = (r.returncode == 0 and any(l.startswith("read: ") for l in lines)
                and got == short)
    print(("ok   " if good else "FAIL ") + path)
    if not good:
        failed += 1
        print("   stdout:", lines[:6], "got", len(got), "bytes", r.stderr[:200])


run("/length", True)
run("/chunked", True)
run("/close", True)
run("/short", False)
srv.shutdown()
print("FAILED" if failed else "passed")
sys.exit(1 if failed else 0)
PY
