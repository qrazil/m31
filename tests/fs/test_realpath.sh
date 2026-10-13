#!/usr/bin/env bash
# fs.realpath and fs.readlink against Python's os.path.realpath(strict=True)
# and os.readlink, on a tree of symbolic links built here: absolute and
# relative links, chains, links to files, a link in the middle of a path
# followed by `..`, dangling links, a loop, and paths that are not there.
# Also timer.monotonic_ns against time.monotonic_ns (the same clock).
#
#   bash tests/fs/test_realpath.sh
#
# Run from the repository root, with the compiler built (`cargo build`).
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./config.sh
. ./runtime/arch.sh

LANGC=./target/debug/$LANG_BIN
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

"$LANGC" fmt --check tests/fs/t_realpath.m31 || { echo "FAILED: fmt"; exit 1; }
"$LANGC" --emit-c tests/fs/t_realpath.m31 -o "$WORK/t.c" 2>/dev/null || { echo "FAILED: compile"; exit 1; }

backends=("")
if [ "$(uname -s)" = Linux ]; then
    case $(uname -m) in x86_64|aarch64|riscv64) backends+=(-DRT_SYS_RAW) ;; esac
fi

fail=0
for flags in "${backends[@]}"; do
    # shellcheck disable=SC2086
    cc -O2 -pthread $flags -I runtime -o "$WORK/t" "$WORK/t.c" \
        runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" || { echo "FAILED: cc"; exit 1; }
    python3 -I - "$WORK/t" "$WORK" "${flags:-libc}" <<'PY' || fail=1
import os, subprocess, sys, time

binary, work, backend = sys.argv[1:4]
# The scratch directory itself may be reached through a link (macOS /var is
# /private/var), which is what the oracle sees too: everything is compared
# after the oracle has resolved it.
root = os.path.join(work, "tree-" + backend.lstrip("-"))
os.makedirs(root)
def mk(*parts): return os.path.join(root, *parts)
os.makedirs(mk("a", "b", "c"))
os.makedirs(mk("other", "deep"))
open(mk("a", "b", "file.txt"), "w").write("x")
os.symlink("b", mk("a", "rel"))                         # relative, to a directory
os.symlink(mk("a", "b", "c"), mk("abs"))                # absolute
os.symlink("rel", mk("a", "chain"))                     # a link to a link
os.symlink("../other/deep", mk("a", "b", "up"))         # relative with ..
os.symlink("b/file.txt", mk("a", "tofile"))             # to a file
os.symlink("nowhere", mk("dangling"))
os.symlink("loop2", mk("loop1"))
os.symlink("loop1", mk("loop2"))
os.symlink(".", mk("self"))
os.symlink("a/b/up/..", mk("viaup"))                    # link then .. : above what up points to
paths = [
    "a", "a/b", "a/b/c", "a/b/file.txt", "a/rel", "a/rel/c", "a/chain", "a/chain/file.txt",
    "abs", "abs/", "abs/..", "a/b/up", "a/b/up/..", "a/tofile", "self", "self/self/a/b",
    "a//b/./c/", "a/b/../b/c", "a/b/c/../../..", "viaup", "dangling", "loop1",
    "nonexistent", "a/nonexistent/c", "a/b/file.txt/more", "a/b/file.txt/..",
    "a/rel/../rel/c",
]
full = [mk(*p.split("/")) if not p.endswith("/") else mk(*p.rstrip("/").split("/")) + "/" for p in paths]
bad = 0
def check(label, ok, detail=""):
    global bad
    print(("ok      " if ok else "FAILED  ") + backend + ": " + label + (("  " + detail) if not ok and detail else ""))
    if not ok:
        bad += 1

def py_real(p):
    try:
        return os.path.realpath(p, strict=True)
    except FileNotFoundError:
        return "error no such file"
    except NotADirectoryError:
        return "error not a directory"
    except OSError as e:
        return "error system error %d" % e.errno
    except RuntimeError:
        return "error system error 40"

out = subprocess.run([binary, "real", *full], capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout.splitlines()
check("one answer per path", len(out) == len(full), str(len(out)))
for p, got in zip(paths, out):
    want = py_real(mk(*p.split("/")) if not p.endswith("/") else mk(*p.rstrip("/").split("/")) + "/")
    check("realpath %s" % p, got == want, "got %r want %r" % (got, want))

# readlink: the text of the link, as written.
links = ["a/rel", "abs", "a/b/up", "dangling", "loop1", "a/b/file.txt", "nonexistent"]
out = subprocess.run([binary, "link", *[mk(*p.split("/")) for p in links]], capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout.splitlines()
for p, got in zip(links, out):
    try:
        want = os.readlink(mk(*p.split("/")))
    except FileNotFoundError:
        want = "error no such file"
    except OSError as e:
        want = "error system error %d" % e.errno
    check("readlink %s" % p, got == want, "got %r want %r" % (got, want))

# A relative path from the current directory (/proc/self/cwd: Linux).
if sys.platform.startswith("linux"):
    done = subprocess.run([binary, "real", "a/rel/c", "../tree/self/a"], cwd=root, capture_output=True, text=True, stdin=subprocess.DEVNULL)
    want = [py_real(os.path.join(root, "a/rel/c")), py_real(os.path.join(root, "../tree/self/a"))]
    check("relative paths", done.stdout.splitlines() == want, "%r vs %r" % (done.stdout, want))

# The monotonic clock is CLOCK_MONOTONIC, which Python reads too: the two
# readings fall between Python's, and the sleep is at least 120 ms.
before = time.monotonic_ns()
done = subprocess.run([binary, "clock"], capture_output=True, text=True, stdin=subprocess.DEVNULL)
after = time.monotonic_ns()
a, b = (int(x) for x in done.stdout.split())
check("monotonic clock is Python's clock", before <= a <= b <= after, "%d %d %d %d" % (before, a, b, after))
check("sleep_ms(120) measured by it is at least 120 ms", b - a >= 120_000_000, str(b - a))
sys.exit(1 if bad else 0)
PY
done
[ $fail -eq 0 ] && echo "realpath: all checks passed"
exit $fail
