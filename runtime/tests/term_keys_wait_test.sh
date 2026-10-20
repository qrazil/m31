#!/usr/bin/env bash
# term.Reader.read_keys: an escape wait that is its own and not the timeout.
#
# runtime/tests/term_keys_wait_test/main.m31 is started with a pipe on standard
# input and run through a list of read_keys / read / sleep calls; this script
# writes bytes into the pipe at chosen times and checks what each call
# returned and when. Every case has a lower bound that is strict (an Escape
# never resolves before its escape wait) and an upper bound that is loose
# (a loaded machine is allowed to be late, not early). Times are the
# milliseconds since the program printed `ready`; the driver's clock starts
# when it reads that line, so the program's times are never smaller than the
# driver's and a lower bound that passes is real.
#
# Usage: bash runtime/tests/term_keys_wait_test.sh
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./config.sh
. ./runtime/arch.sh

LANGC=${LANGC:-./target/debug/$LANG_BIN}
if [ ! -x "$LANGC" ]; then
    echo "compiler not built: $LANGC" >&2
    exit 1
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

if ! "$LANGC" -emit-c runtime/tests/term_keys_wait_test/main.m31 --out "$WORK/t.c" 2>"$WORK/t.err"; then
    echo "FAILED: compile: $(grep -v '^warning' "$WORK/t.err" | head -5)"
    exit 1
fi
if ! cc -O2 -pthread -I runtime -o "$WORK/t" "$WORK/t.c" \
        runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" 2>"$WORK/t.cc"; then
    echo "FAILED: build:"
    sed 's/^/    /' "$WORK/t.cc" | head -20
    exit 1
fi

python3 -I - "$WORK/t" <<'PY'
import os, subprocess, sys, time

program = sys.argv[1]
failures = 0

def run(calls, writes, close_at=None):
    """Run the program with `calls` (list of (kind, a, b)); `writes` is a
    list of (ms, bytes) and `close_at` the ms at which stdin is closed.
    Returns (rows, write_times): rows are (outcome, pending, t_ms) and
    write_times the driver's own ms reading just after each write."""
    argv = [program]
    for kind, first, second in calls:
        argv += [kind, str(first), str(second)]
    env = dict(os.environ, LANG_NUM_CARRIERS="1")
    child = subprocess.Popen(argv, stdin=subprocess.PIPE, stderr=subprocess.PIPE,
                             stdout=subprocess.DEVNULL, env=env)
    ready = child.stderr.readline()
    assert ready.strip() == b"ready", ready
    start = time.monotonic()
    def now():
        return (time.monotonic() - start) * 1000.0
    events = [(ms, "w", data) for ms, data in writes]
    if close_at is not None:
        events.append((close_at, "c", None))
    events.sort(key=lambda e: e[0])
    wrote = []
    for ms, kind, data in events:
        delay = ms - now()
        if delay > 0:
            time.sleep(delay / 1000.0)
        if kind == "w":
            try:
                child.stdin.write(data)
                child.stdin.flush()
            except BrokenPipeError:
                pass   # the program is already gone: the checks below say so
            wrote.append(now())
        else:
            child.stdin.close()
    rest = child.stderr.read()
    try:
        child.wait(timeout=30)
    except subprocess.TimeoutExpired:
        child.kill()
        child.wait()
    if close_at is None:
        child.stdin.close()
    rows = []
    for line in rest.decode().splitlines():
        parts = line.split(" ")
        # "<n> <outcome words...> pending=<p> t=<ms>"
        rows.append((" ".join(parts[1:-2]), int(parts[-2].split("=")[1]),
                     int(parts[-1].split("=")[1])))
    return rows, wrote

def check(name, ok, detail):
    global failures
    if ok:
        print("ok: " + name)
    else:
        failures += 1
        print("FAILED: " + name + ": " + detail)

def between(name, rows, wrote, index, outcome, pending, low, high):
    """Row `index` is `outcome` with `pending` bytes held, at a time in
    [low, high] ms on the driver's clock (strict low, loose high; t is
    truncated to whole milliseconds, so the low bound has 1 ms of slack)."""
    if index >= len(rows):
        check(name, False, "only %d rows: %r" % (len(rows), rows))
        return
    got, held, t = rows[index]
    # The program's t is truncated to whole milliseconds, hence the 1.
    ok = got == outcome and held == pending and low - 1 <= t <= high
    check(name, ok, "row %d is %r pending=%d t=%d; wanted %r pending=%d in [%d, %d]; rows %r"
          % (index, got, held, t, outcome, pending, low, high, rows))

SLACK = 500   # loose upper bound, ms
E = 150

# 1. A lone ESC is resolved after escape_ms, not before, though the timeout
#    is for ever.
rows, w = run([("k", -1, E), ("k", 300, E)], [(100, b"\x1b")])
between("lone ESC: escape after escape_ms, not before", rows, w, 0, "got escape", 0,
        w[0] + E, w[0] + E + SLACK)
between("nothing pending afterwards: plain timeout", rows, w, 1, "timeout", 0, w[0] + E + 300 - 5, w[0] + E + 300 + SLACK)

# 2. ESC [ A with both gaps below escape_ms: one key, at the last byte.
# (A long escape wait: a loaded runner may deliver a write well after its
# nominal time, and the point is only that the gaps are below escape_ms.)
rows, w = run([("k", -1, 1500)], [(100, b"\x1b"), (160, b"["), (220, b"A")])
between("split ESC [ A, gaps below escape_ms: up, at the last byte", rows, w, 0, "got up", 0,
        w[2], w[2] + SLACK)

# 3. The same with a gap above escape_ms: Escape first, then the rest.
rows, w = run([("k", -1, E), ("k", -1, E), ("k", -1, E)],
              [(100, b"\x1b"), (400, b"["), (410, b"A")])
between("gap above escape_ms: Escape resolved after escape_ms", rows, w, 0, "got escape", 0,
        w[0] + E, w[1] - 1)
check("gap above escape_ms: then [ and A as plain keys",
      [r[0] for r in rows[1:]] == ["got [", "got A"],
      "rows %r" % (rows,))

# 4. Timeout with nothing pending is Timeout, at timeout_ms (not escape_ms),
#    and flushes nothing: an ESC written afterwards is still a whole wait.
rows, w = run([("k", 200, 50), ("k", -1, 50)], [(300, b"\x1b")])
between("nothing pending: Timeout after timeout_ms", rows, w, 0, "timeout", 0, 200, 200 + SLACK)
between("an ESC after that waits its own escape_ms", rows, w, 1, "got escape", 0, w[0] + 50, w[0] + 50 + SLACK)

# 5. Continuous input restarts the escape wait: six bytes 120 ms apart with
#    escape_ms 500 are one key, though the whole is longer than escape_ms.
rows, w = run([("k", -1, 500)],
              [(100, b"\x1b"), (220, b"["), (340, b"1"), (460, b";"), (580, b"5"), (700, b"A")])
between("continuous input restarts the wait: one ctrl+up", rows, w, 0, "got ctrl+up", 0, w[5], w[5] + SLACK)

# 6. timeout_ms shorter than escape_ms with a prefix pending: Timeout, the
#    prefix stays held, and the NEXT call continues the same escape wait.
#    ESC arrives at 100 and the escape wait is 1500, so it ends at 1600 -- not
#    at 2000, which a restarted wait would give.
#    (Timeouts well after the write: a loaded runner may start the driver's
#    clock late, and the ESC must have arrived before the first timeout.)
rows, w = run([("k", 500, 1500), ("k", 400, 1500), ("k", -1, 1500)], [(100, b"\x1b")])
between("short timeout, prefix held: Timeout at timeout_ms", rows, w, 0, "timeout", 1, 500, 500 + SLACK)
between("short timeout again: Timeout, still held", rows, w, 1, "timeout", 1, 900, 900 + SLACK)
between("the escape wait continued, not restarted", rows, w, 2, "got escape", 0, w[0] + 1500, w[0] + 1500 + 250)

# 7. timeout_ms 0: nothing arrived -> immediate Timeout; a held prefix whose
#    wait is still running stays; one whose wait has run out is resolved.
rows, w = run([("k", 0, E), ("k", 700, 5000), ("k", 0, 5000), ("k", 0, 5000)], [(100, b"\x1b")])
between("timeout 0 with nothing: immediate Timeout", rows, w, 0, "timeout", 0, 0, 100 - 1 + SLACK)
between("timeout 0, prefix with wait running: Timeout", rows, w, 2, "timeout", 1, 700, 700 + SLACK)
rows, w = run([("k", 800, 5000), ("k", 0, 300)], [(100, b"\x1b")])
between("timeout 0, escape wait already run out: resolved at once", rows, w, 1, "got escape", 0,
        800, 800 + SLACK)

# 8. escape_ms 0 resolves as soon as nothing more can be read; a negative
#    escape_ms never resolves (only the end of the input does).
rows, w = run([("k", -1, 0)], [(100, b"\x1b")])
between("escape_ms 0: resolved at once", rows, w, 0, "got escape", 0, w[0], w[0] + SLACK)
rows, w = run([("k", 500, -1), ("k", -1, -1), ("k", -1, -1)], [(100, b"\x1b")], close_at=700)
between("negative escape_ms: the prefix is held", rows, w, 0, "timeout", 1, 500, 500 + SLACK)
between("negative escape_ms: the end resolves it", rows, w, 1, "got escape", 0, 700, 700 + SLACK)
between("negative escape_ms: then the end", rows, w, 2, "end", 0, 700, 700 + SLACK)

# 9. A prefix held when the call is made (left by read(0), say) starts its
#    wait at the call, not at the arrival: ESC at 100, the call at 300, so
#    the escape comes at 300 + 200 and not at 100 + 200.
rows, w = run([("s", 300, 0), ("r", 0, 0), ("k", -1, 200)], [(100, b"\x1b")])
between("read(0) leaves the prefix held", rows, w, 0, "timeout", 1, 300, 300 + SLACK)
between("a held prefix starts its wait at the call", rows, w, 1, "got escape", 0, 300 + 200, 300 + 200 + SLACK)

sys.exit(1 if failures else 0)
PY
status=$?
if [ "$status" -ne 0 ]; then
    exit 1
fi
echo "ok: read_keys keeps its escape wait apart from the timeout"
