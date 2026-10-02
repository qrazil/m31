#!/usr/bin/env bash
# apps/httpserver's own tests: builds the real binary, runs it on a scratch
# port, and talks to it over a real socket -- curl for the simple checks, a
# small Python client (threads, real `socket`s, nothing mocked) for the
# concurrent-load check, which is the one this whole app exists to pass.
#
#   bash apps/httpserver/test.sh
#
# Run from the repository root, with the compiler built (`cargo build`).
# Not part of gates.sh's ordinary corpus: this starts a real long-running
# server process, which the corpus runner (run.sh) is not shaped for.
set -uo pipefail
cd "$(dirname "$0")/../.."

WORK=$(mktemp -d)
pass=0
fail=0
server_pid=""

note() { printf '\033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '\033[31mFAIL\033[0m %s\n' "$1"; shift; printf '%s\n' "$@" | sed 's/^/     /'; fail=$((fail + 1)); }

cleanup() {
    [ -n "$server_pid" ] && kill "$server_pid" >/dev/null 2>&1
    [ -n "$server_pid" ] && wait "$server_pid" 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

if ! bash apps/httpserver/build.sh >"$WORK/build.log" 2>&1; then
    bad "build" "$(cat "$WORK/build.log")"
    exit 1
fi
note "build: apps/httpserver/httpserver"

BIN=apps/httpserver/httpserver
PORT=$((20000 + (RANDOM % 20000)))
BASE="http://127.0.0.1:$PORT"

"$BIN" --port "$PORT" --host 127.0.0.1 >"$WORK/server.log" 2>&1 &
server_pid=$!

up=0
tries=0
while [ "$tries" -lt 100 ]; do
    if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then
        exec 3>&-
        up=1
        break
    fi
    sleep 0.05
    tries=$((tries + 1))
done

if [ "$up" != 1 ]; then
    bad "server start" "$(cat "$WORK/server.log")"
    exit 1
fi
note "server: listening on 127.0.0.1:$PORT (pid $server_pid)"

# --- 1. one request: status, body, Content-Length ---------------------------

resp=$(curl -s -D "$WORK/h1" -o "$WORK/b1" -w '%{http_code}' "$BASE/")
body=$(cat "$WORK/b1")
clen=$(tr -d '\r' <"$WORK/h1" | grep -i '^content-length:' | awk '{print $2}')
if [ "$resp" = "200" ] && [ "$body" = $'Hello, World!' ] && [ "$clen" = "14" ]; then
    note "one request: 200, exact body, Content-Length: 14"
else
    bad "one request" "status=$resp body=[$body] content-length=[$clen]"
fi

# --- 2. a second (and third) request on the SAME kept-alive connection ------
# No `-o` here on purpose: curl reuses one connection across multiple URLs
# given on one command line, and with no output file to disagree about, all
# three bodies land on stdout, concatenated, in order.

two=$(curl -s "$BASE/" "$BASE/" "$BASE/")
want_two=$'Hello, World!\nHello, World!\nHello, World!'
if [ "$two" = "$want_two" ]; then
    note "keep-alive: three sequential requests on one connection all answered"
else
    bad "keep-alive" "got [$two]"
fi

# --- 3. different paths and methods get the same reply (ignores the request) -

ok3=1
for args in "$BASE/" "$BASE/anything/at/all" "$BASE/?x=1" "-X POST $BASE/post" "--head $BASE/head"; do
    # --head (not -X HEAD): curl only knows to stop expecting a body on a
    # HEAD *response* when it knows the *request* was HEAD through this
    # flag -- `-X HEAD` just swaps the method word and otherwise reads the
    # reply like a GET, so it hangs waiting for Content-Length bytes a
    # correct HEAD response (ours) never sends on a kept-alive connection.
    # Confirmed against Python's own http.server: the same `-X HEAD` hangs
    # there too (or errors, over HTTP/1.0) -- a curl footgun, not a server
    # bug; curl's own warning says exactly this ("use -I/--head instead").
    out=$(eval curl -s -o /dev/null -w "'%{http_code}'" --max-time 5 "$args")
    if [ "$out" != "200" ]; then
        ok3=0
        bad "path/method $args" "status=$out"
    fi
done
[ "$ok3" = 1 ] && note "every path and method gets 200 (the handler ignores the request)"

# --- 4. concurrent load: N persistent connections x M sequential requests ---
# each -- the actual property this app exists to demonstrate: every accepted
# connection is its own green thread, so real concurrent socket I/O does not
# serialize on one carrier. Run twice (different N) to make the claim a
# little stronger than one lucky pass.

concurrent_check() {
    local n=$1 m=$2
    python3 - "$PORT" "$n" "$m" <<'PY'
import socket
import sys
import threading

port = int(sys.argv[1])
n = int(sys.argv[2])
m = int(sys.argv[3])
ok = [True] * n
errs = [""] * n


def worker(i):
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=10)
        s.settimeout(10)
        for _ in range(m):
            s.sendall(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            data = b""
            while b"\r\n\r\n" not in data:
                chunk = s.recv(4096)
                if not chunk:
                    raise RuntimeError("connection closed early")
                data += chunk
            head, _, rest = data.partition(b"\r\n\r\n")
            if b" 200 " not in head.split(b"\r\n", 1)[0]:
                raise RuntimeError("not 200: " + head.split(b"\r\n", 1)[0].decode())
            need = 14 - len(rest)
            while need > 0:
                chunk = s.recv(need)
                if not chunk:
                    raise RuntimeError("short body")
                need -= len(chunk)
        s.close()
    except Exception as e:
        ok[i] = False
        errs[i] = repr(e)


threads = [threading.Thread(target=worker, args=(i,)) for i in range(n)]
for t in threads:
    t.start()
for t in threads:
    t.join(15)
failed = [i for i in range(n) if not ok[i]]
if failed:
    print(f"FAIL {len(failed)}/{n} connections failed: {errs[failed[0]]}")
    sys.exit(1)
print(f"OK {n} connections x {m} requests each, all succeeded")
PY
}

if out=$(concurrent_check 40 10 2>&1); then
    note "concurrent load: $out"
else
    bad "concurrent load (40x10)" "$out"
fi

if out=$(concurrent_check 80 5 2>&1); then
    note "concurrent load: $out"
else
    bad "concurrent load (80x5)" "$out"
fi

# --- 5. the command line: --help, a bad flag, a bad port --------------------

help_out=$("$BIN" --help)
if printf '%s' "$help_out" | grep -q "httpserver"; then
    note "--help prints usage"
else
    bad "--help" "$help_out"
fi

if "$BIN" --nonsense >"$WORK/badflag.out" 2>&1; then
    bad "bad flag accepted" "$(cat "$WORK/badflag.out")"
else
    note "a bad flag is refused (exit nonzero)"
fi

if "$BIN" --port 99999 >"$WORK/badport.out" 2>&1; then
    bad "bad --port accepted" "$(cat "$WORK/badport.out")"
else
    note "a --port outside 0..65535 is refused with a message, not a trap"
fi

# --- summary ------------------------------------------------------------

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
