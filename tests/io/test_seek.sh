#!/usr/bin/env bash
# `io.File.seek` and `io.File.tell` against Python's raw file object.
#
# A 200000-byte file of random bytes (so the 64 KiB read buffer is crossed
# three times) and several hundred random operations -- absolute, relative and
# from-the-end seeks, including past the end and to a negative position, tells
# and reads -- are run through tests/io/t_seek.m31 and through Python's
# `open(path, "rb", buffering=0)`, which keeps no read-ahead and so is the
# oracle for "where the next read comes from". Output must agree line by line.
#
#   bash tests/io/test_seek.sh
#
# Run from the repository root, with the compiler built (`cargo build`).
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh

LANGC=./target/debug/m31c
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

"$LANGC" -fmt --check tests/io/t_seek.m31 || { echo "FAILED: fmt"; exit 1; }
"$LANGC" -emit-c tests/io/t_seek.m31 --out "$WORK/s.c" || exit 1
cc -O2 -I runtime -pthread -o "$WORK/s" "$WORK/s.c" \
    runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" || exit 1

python3 -I - "$WORK" <<'PY'
import os, random, subprocess, sys

work = sys.argv[1]
path = os.path.join(work, "data.bin")
rng = random.Random(31)
size = 200000
with open(path, "wb") as fh:
    fh.write(bytes(rng.randrange(256) for _ in range(size)))

failed = 0
for trial in range(20):
    ops = []
    for _ in range(40):
        k = rng.choice("SCERRRT")
        if k == "T":
            ops.append("T")
        elif k == "R":
            ops.append("R:%d" % rng.choice([1, 7, 100, 4096, 70000]))
        else:
            span = rng.choice([10, 1000, 70000, size + 5000])
            lo = -span if k != "S" else -3
            ops.append("%s:%d" % (k, rng.randrange(lo, span + 1)))
    got = subprocess.run([os.path.join(work, "s"), path] + ops,
                         capture_output=True, text=True, timeout=60)
    want = []
    with open(path, "rb", buffering=0) as f:
        for op in ops:
            if op == "T":
                want.append(str(f.tell()))
            elif op[0] == "R":
                n = int(op[2:])
                buf = b""
                while len(buf) < n:
                    part = f.read(n - len(buf))
                    if not part:
                        break
                    buf += part
                want.append(buf.hex())
            else:
                whence = {"S": 0, "C": 1, "E": 2}[op[0]]
                try:
                    want.append(str(f.seek(int(op[2:]), whence)))
                except OSError:
                    want.append("ERR")
    if got.returncode != 0 or got.stdout.split("\n")[:-1] != want:
        failed += 1
        print("FAIL trial %d (exit %d)" % (trial, got.returncode))
        for i, (a, b) in enumerate(zip(got.stdout.split("\n"), want)):
            if a != b:
                print("  op %s: m31 %r python %r" % (ops[i], a[:60], b[:60]))
                break
        print(got.stderr[:300])
print("FAILED" if failed else "passed: 20 trials x 40 operations match Python")
sys.exit(1 if failed else 0)
PY
