#!/usr/bin/env bash
# term.Reader.read waits through the reactor: only the green thread parks.
#
# runtime/term_wait_test/main.m31 starts a ticker thread and then waits 400 ms
# for a key on a pty. On ONE carrier, a wait that held the carrier would run
# the ticker only after the read returned, so the program prints whether the
# ticker finished first. Then keys are typed into the pty and read back.
# A second run has stdin on /dev/null, a descriptor the reactor cannot wait on
# (-EPERM), which must read as ready and end.
#
# Usage: bash runtime/term_wait_test.sh
set -uo pipefail
cd "$(dirname "$0")/.."
. ./config.sh
. ./runtime/arch.sh

LANGC=${LANGC:-./target/debug/$LANG_BIN}
if [ ! -x "$LANGC" ]; then
    echo "compiler not built: $LANGC" >&2
    exit 1
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

if ! "$LANGC" --emit-c runtime/term_wait_test/main.m31 -o "$WORK/t.c" 2>"$WORK/t.err"; then
    echo "FAILED: compile: $(grep -v '^warning' "$WORK/t.err" | head -5)"
    exit 1
fi
if ! cc -O2 -pthread -I runtime -o "$WORK/t" "$WORK/t.c" \
        runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" 2>"$WORK/t.cc"; then
    echo "FAILED: build:"
    sed 's/^/    /' "$WORK/t.cc" | head -20
    exit 1
fi

fail=0
export LANG_NUM_CARRIERS=1
out=$(bash runtime/with_timeout.sh 20 "$WORK/t" </dev/null 2>&1)
if [ "$out" = "not a tty: end" ]; then
    echo "ok: stdin on /dev/null reads as ready and ends"
else
    echo "FAILED: /dev/null run printed: $out"
    fail=1
fi

python3 - "$WORK/t" <<'PY' || fail=1
import os, pty, select, sys, time

pid, fd = pty.fork()
if pid == 0:
    os.execv(sys.argv[1], [sys.argv[1]])
buf = b""

def pump(until, limit):
    global buf
    end = time.time() + limit
    while until not in buf and time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.1)
        if r:
            try:
                data = os.read(fd, 4096)
            except OSError:
                return
            if not data:
                return
            buf += data

pump(b"ready", 10)
pump(b"the ticker ran during the wait", 10)
os.write(fd, b"\x1b[A")
pump(b"got ", 10)
os.write(fd, b"q")
pump(b"done", 10)
text = buf.decode("utf-8", "replace").replace("\r", "")
want = [
    "timeout",
    "waited at least 400 ms: true",
    "the ticker ran during the wait: true",
    "got up",
    "got q",
    "done",
]
bad = [w for w in want if w not in text]
try:
    os.waitpid(pid, 0)
except ChildProcessError:
    pass
if bad:
    print("FAILED: missing", bad)
    print(text)
    sys.exit(1)
print("ok: a pty wait times out, parks only the green thread, and reads keys")
PY
exit $fail
