#!/usr/bin/env bash
# fs.realpath and fs.readlink against Python's os.path.realpath(strict=True)
# and os.readlink, on a tree of symbolic links built here: absolute and
# relative links, chains, links to files, a link in the middle of a path
# followed by `..`, dangling links, a loop, and paths that are not there.
# A relative path and fs.getcwd (no /proc: the same on macOS): a lying or
# missing PWD, a path of 3.4 KB, one past the kernel's limit, and a removed
# current directory. Also timer.monotonic_ns against time.monotonic_ns.
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

"$LANGC" -fmt --check tests/fs/t_realpath.m31 || { echo "FAILED: fmt"; exit 1; }
"$LANGC" -emit-c tests/fs/t_realpath.m31 --out "$WORK/t.c" 2>/dev/null || { echo "FAILED: compile"; exit 1; }

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
    ok = got == want
    # errno numbers are the layer's (Linux): macOS's Python reports its own.
    if not ok and want.startswith("error system error "):
        names = {62: 40, 63: 36, 20: 20}
        ok = got == "error system error %d" % names.get(int(want.split()[-1]), -1)
    # `file/..`: the kernel and newer Pythons say ENOTDIR; Python 3.12 takes
    # the parent lexically, as the library does. Either is the library's.
    if not ok and p.endswith("/file.txt/.."):
        ok = want == "error not a directory" and got == py_real(os.path.dirname(mk(*p.split("/")[:-1])))
    check("realpath %s" % p, ok, "got %r want %r" % (got, want))

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

# A relative path from the current directory: fs.getcwd, on every system
# (no /proc). The child runs with a hostile environment too: no PWD at all,
# and a PWD that names somewhere else, neither of which may matter.
done = subprocess.run([binary, "real", "a/rel/c", "../tree/self/a"], cwd=root, capture_output=True, text=True, stdin=subprocess.DEVNULL)
want = [py_real(os.path.join(root, "a/rel/c")), py_real(os.path.join(root, "../tree/self/a"))]
check("relative paths", done.stdout.splitlines() == want, "%r vs %r" % (done.stdout, want))
real_root = os.path.realpath(root)
def run_cwd(env, **kw):
    done = subprocess.run([binary, "cwd"], capture_output=True, text=True, stdin=subprocess.DEVNULL, env=env, **kw)
    return done.stdout.splitlines()
check("getcwd and realpath('.') are the directory", run_cwd({}, cwd=root) == [real_root, real_root], str(run_cwd({}, cwd=root)))
check("getcwd ignores a lying PWD", run_cwd({"PWD": "/nonexistent/lie"}, cwd=root) == [real_root, real_root])
check("getcwd through a link is the real directory", run_cwd({}, cwd=mk("abs")) == [os.path.realpath(mk("abs"))] * 2)
check("getcwd of /", run_cwd({}, cwd="/") == ["/", "/"])
done = subprocess.run([binary, "real", "a/b/c/../file.txt", "."], cwd=root, capture_output=True, text=True, stdin=subprocess.DEVNULL, env={})
check("relative paths with no environment at all", done.stdout.splitlines() == [py_real(mk("a/b/file.txt")), real_root], repr(done.stdout))

# A long path: the process is put in the directory by a chdir per component
# (no path this long can be passed to one), 3.4 KB of path; and one past the
# kernel's 4096, where the raw backend says ENAMETOOLONG (36) and a C library
# whose getcwd walks up with `..` may still answer.
def deep(names):
    def go():
        for name in names:
            os.chdir(name)
    return go
def build_deep(base, count, width):
    os.makedirs(base)
    names = [("d%02d" % i).ljust(width, "x") for i in range(count)]
    here = os.getcwd()
    try:
        os.chdir(base)
        for name in names:
            os.mkdir(name)
            os.chdir(name)
    finally:
        os.chdir(here)
    return names
near = build_deep(mk("deepnear"), 17, 200)
want = os.path.realpath(mk("deepnear")) + "/" + "/".join(near)
check("long path under PATH_MAX is built (%d bytes)" % len(want), 3000 < len(want) < 4096, str(len(want)))
done = subprocess.run([binary, "cwd"], cwd=mk("deepnear"), preexec_fn=deep(near), capture_output=True, text=True, stdin=subprocess.DEVNULL)
check("getcwd in a %d-byte path" % len(want), done.stdout.splitlines() == [want, want], repr(done.stdout[:200]))
done = subprocess.run([binary, "real", "../" + near[-1] + "/../" + near[-1]], cwd=mk("deepnear"), preexec_fn=deep(near), capture_output=True, text=True, stdin=subprocess.DEVNULL)
check("a relative realpath in the long directory", done.stdout.splitlines() == [want], repr(done.stdout[:200]))
far = build_deep(mk("deepfar"), 22, 200)
farwant = os.path.realpath(mk("deepfar")) + "/" + "/".join(far)
done = subprocess.run([binary, "cwd"], cwd=mk("deepfar"), preexec_fn=deep(far), capture_output=True, text=True, stdin=subprocess.DEVNULL)
lines = done.stdout.splitlines()
if backend == "-DRT_SYS_RAW":
    check("getcwd past PATH_MAX is ENAMETOOLONG on the raw backend", lines == ["error system error 36", "error system error 36"], repr(done.stdout[:200]))
else:
    # (realpath(".") then stats the whole path, which no system accepts.)
    check("getcwd past PATH_MAX is the host's answer, whole or an error, never cut",
          len(lines) == 2 and all(l == farwant or l.startswith("error ") for l in lines), repr(done.stdout[:200]))

# A current directory that was removed under the process: NotFound, never a
# stale name; an absolute path still works.
os.makedirs(mk("doomed"))
def vanish():
    os.chdir(mk("doomed"))
    os.rmdir(mk("doomed"))
done = subprocess.run([binary, "cwd"], preexec_fn=vanish, capture_output=True, text=True, stdin=subprocess.DEVNULL)
check("getcwd in a removed directory is NotFound", done.stdout.splitlines() == ["error no such file", "error no such file"], repr(done.stdout))
os.makedirs(mk("doomed"))
done = subprocess.run([binary, "real", "x", "/", "a/b"], preexec_fn=vanish, capture_output=True, text=True, stdin=subprocess.DEVNULL)
check("realpath of a relative path there is NotFound, an absolute one works",
      done.stdout.splitlines() == ["error no such file", "/", "error no such file"], repr(done.stdout))

# The monotonic clock is CLOCK_MONOTONIC, which Python reads too: the two
# readings fall between Python's, and the sleep is at least 120 ms.
before = time.monotonic_ns()
done = subprocess.run([binary, "clock"], capture_output=True, text=True, stdin=subprocess.DEVNULL)
after = time.monotonic_ns()
a, b = (int(x) for x in done.stdout.split())
# macOS's Python reads a different clock (mach uptime) than CLOCK_MONOTONIC.
same_clock = before <= a <= b <= after
if sys.platform == "darwin":
    same_clock = a <= b
check("monotonic clock is Python's clock", same_clock, "%d %d %d %d" % (before, a, b, after))
check("sleep_ms(120) measured by it is at least 120 ms", b - a >= 120_000_000, str(b - a))
sys.exit(1 if bad else 0)
PY
done
[ $fail -eq 0 ] && echo "realpath: all checks passed"
exit $fail
