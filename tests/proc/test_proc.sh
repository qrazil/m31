#!/usr/bin/env bash
# lib/proc.m31 against Python's subprocess module and coreutils.
#
# tests/proc/t_proc.m31 is built once per sys-layer backend (libc, and the raw
# Linux system calls where this machine has them) and driven through:
#
#   parity    ~30 commands -- exit codes, death by signal, stderr apart from
#             stdout, binary output, env=, cwd=, programs that are missing or
#             cannot be run -- through proc.run AND proc.start, each line
#             compared with what subprocess.run says about the same command
#   copy      a file of random bytes through `cat` with io.copy, a thread
#             feeding while this one drains: sha256 equal, and the process's
#             resident set the same for 24 MiB as for 200 MiB (constant
#             memory, which is the whole point of a pipe)
#   input     8 MiB of stdin, far past the pipe buffer, with output being
#             drained at the same time: no deadlock, sha256 equal
#   cap       70 MB of output is kept to 64 MiB and says so
#   timeout   a timed-out run is TimedOut, in time
#   epipe     a write after the child has exited is BrokenPipe
#   pty       the same commands on a pseudo-terminal through proc.start_pty and
#             through Python's pty module: tty, stty size, isatty, an
#             interactive cat (echo and ^D), a raw-mode program, ^C, a resize
#
#   bash tests/proc/test_proc.sh
#
# Run from the repository root, with the compiler built (`cargo build`).
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./config.sh
. ./runtime/arch.sh

LANGC=./target/debug/$LANG_BIN
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

"$LANGC" fmt --check tests/proc/t_proc.m31 || { echo "FAILED: fmt"; exit 1; }
"$LANGC" --emit-c tests/proc/t_proc.m31 -o "$WORK/t.c" 2>/dev/null || { echo "FAILED: compile"; exit 1; }

backends=("libc:")
if [ "$(uname -s)" = Linux ]; then
    case $(uname -m) in x86_64|aarch64|riscv64) backends+=("raw:-DRT_SYS_RAW") ;; esac
fi

python3 -I - "$WORK" <<'PY'
import hashlib, os, random, sys
work = sys.argv[1]
rng = random.Random(31)
def make(name, size):
    block = bytes(rng.randrange(256) for _ in range(1 << 16))
    with open(os.path.join(work, name), "wb") as fh:
        left = size
        i = 0
        while left > 0:
            chunk = block[i % 251:] + block[:i % 251]
            n = min(left, len(chunk))
            fh.write(chunk[:n])
            left -= n
            i += 1
make("small.bin", 24 << 20)
make("large.bin", 200 << 20)
make("input.bin", 8 << 20)
PY

failed=0
for entry in "${backends[@]}"; do
    name=${entry%%:*}
    flags=${entry#*:}
    bin="$WORK/t-$name"
    # shellcheck disable=SC2086
    cc -O2 -pthread $flags -I runtime -o "$bin" "$WORK/t.c" \
        runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" || { echo "FAILED: cc ($name)"; exit 1; }
    python3 -I - "$bin" "$WORK" "$name" <<'PY' || failed=1
import hashlib, os, resource, subprocess, sys, time

binary, work, backend = sys.argv[1:4]
bad = 0

def check(label, ok, detail=""):
    global bad
    print(("ok      " if ok else "FAILED  ") + backend + ": " + label + (("  " + detail) if detail and not ok else ""))
    if not ok:
        bad += 1

def say(rc, out, err):
    return "run %d %s %s" % (rc, out.hex(), err.hex())

def oracle(spec):
    argv = []
    cwd = None
    env = None
    command = False
    for word in spec:
        if command:
            argv.append(word)
        elif word == "--":
            command = True
        elif word.startswith("cwd="):
            cwd = word[4:]
        elif word.startswith("env="):
            k, v = word[4:].split("=", 1)
            env = dict(env or {}, **{k: v})
    try:
        done = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, stdin=subprocess.DEVNULL)
    except FileNotFoundError:
        return "run error program or directory not found"
    except PermissionError:
        return "run error permission denied"
    except NotADirectoryError:
        return "run error working directory is not a directory"
    return say(done.returncode, done.stdout, done.stderr).rstrip()

def m31(*args, timeout=120):
    done = subprocess.run([binary, *args], capture_output=True, timeout=timeout, stdin=subprocess.DEVNULL)
    return done.stdout.decode().strip(), done.returncode, done.stderr.decode()

sh = lambda text: ["--", "sh", "-c", text]
specs = [
    ["--", "echo", "hello", "world"],
    ["--", "true"],
    ["--", "false"],
    sh("exit 7"),
    sh("exit 255"),
    sh("echo out; echo err >&2; exit 2"),
    sh("kill -9 $$"),
    sh("kill -TERM $$"),
    sh("kill -ABRT $$"),
    sh("printf '\\000\\001\\377\\n'"),
    sh("head -c 300000 /dev/zero | tr '\\000' 'x'; head -c 200000 /dev/zero 1>&2"),
    ["cwd=/", "--", "pwd"],
    ["cwd=/tmp", "--", "sh", "-c", "pwd; ls -d ."],
    ["env=A=1", "env=B=two", "--", "sh", "-c", "echo [$A$B] [$HOME]"],
    ["env=PATH=/bin:/usr/bin", "env=X=y", "--", "sh", "-c", "echo $X"],
    ["env=ONLY=this", "--", "sh", "-c", "echo $ONLY"],
    ["--", "no-such-program-2026"],
    ["--", "/no/such/program-2026"],
    ["--", "/etc/passwd"],
    ["cwd=/no/such/dir-2026", "--", "true"],
    ["cwd=/etc/passwd", "--", "true"],
    sh("cat; echo done"),
    sh("sleep 0.05; echo late"),
]
for spec in specs:
    want = oracle(spec)
    for mode in ("run", "start"):
        got, rc, err = m31(mode, *spec)
        # The two failure sentences are this library's own words for a
        # failure, so they are compared as sentences.
        want_line = want
        if mode == "start" and want.startswith("run error"):
            want_line = want
        check("%s %s" % (mode, " ".join(spec)[:60]), got == want_line, "got %r want %r" % (got[:120], want_line[:120]))

# A NotADirectory for a file as cwd is spelled differently by libraries;
# what matters is that it is an error and no process ran.

def sha(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        while True:
            block = fh.read(1 << 20)
            if not block:
                break
            h.update(block)
    return h.hexdigest()

rss = {}
for name in ("small", "large"):
    src = os.path.join(work, name + ".bin")
    dst = os.path.join(work, name + ".out")
    before = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    out, rc, err = m31("copy", src, dst, timeout=600)
    size = os.path.getsize(src)
    check("copy %s: exit, counts" % name, rc == 0 and ("drained %d" % size) in out and ("fed %d" % size) in out and "status exited 0" in out, out + err)
    check("copy %s: sha256 equal" % name, sha(src) == sha(dst))
    rss[name] = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    os.unlink(dst)
unit = 1 if sys.platform == "darwin" else 1024
check("memory: 200 MiB moved with a resident set under 48 MiB", rss["large"] * unit < 48 << 20, "%d bytes" % (rss["large"] * unit))
check("memory: the same for 24 MiB and 200 MiB", rss["large"] - rss["small"] <= 8 * 1024 * 1024 // unit,
      "small %d large %d" % (rss["small"], rss["large"]))

src = os.path.join(work, "input.bin")
dst = os.path.join(work, "input.out")
t0 = time.time()
out, rc, err = m31("run-input", src, dst)
want = subprocess.run(["cat"], input=open(src, "rb").read(), capture_output=True).stdout
check("8 MiB of input beside draining: no deadlock, sha256 equal", rc == 0 and sha(dst) == hashlib.sha256(want).hexdigest(), out + err)

out, rc, err = m31("cap")
check("output is capped at 64 MiB and says so", out == "kept 67108864 truncated true rc 0", out + err)

out, rc, err = m31("timeout", "300")
check("a timed-out run is TimedOut, and in time", out == "error timed out within true", out + err)

out, rc, err = m31("write-after-exit")
try:
    p = subprocess.Popen(["true"])
    p.wait()
    try:
        p.stdin  # not a pipe: python needs one
    except Exception:
        pass
    p2 = subprocess.Popen(["true"], stdin=subprocess.PIPE)
    p2.wait()
    try:
        p2.stdin.write(b"late")
        p2.stdin.flush()
        py = "wrote"
    except BrokenPipeError:
        py = "write error the process closed its input"
except Exception as exc:
    py = "?%r" % exc
check("a write after the child exited is BrokenPipe", out.splitlines()[-1] == py, out + err)

# ---- pseudo-terminals, against Python's pty module ----
import fcntl, pty, re, select, struct, termios

def pty_oracle(argv, rows=24, cols=80, until=b"", typed=b""):
    pid, fd = pty.fork()
    if pid == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        try:
            os.execvp(argv[0], argv)
        finally:
            os._exit(127)
    seen = b""
    def pull():
        try:
            return os.read(fd, 4096)
        except OSError:
            return b""
    ended = False
    while until and until not in seen and not ended:
        chunk = pull()
        if not chunk:
            ended = True
        seen += chunk
    if typed:
        os.write(fd, typed)
    while True:
        chunk = pull()
        if not chunk:
            break
        seen += chunk
    _, st = os.waitpid(pid, 0)
    code = os.waitstatus_to_exitcode(st)
    os.close(fd)
    return code, seen

def norm(data):
    return re.sub(rb"/dev/(pts/\d+|tty[a-z0-9]+)", b"/dev/pts/N", data)

def m31_pty(argv, rows=24, cols=80, until="", typed=b""):
    words = ["pty", "rows=%d" % rows, "columns=%d" % cols]
    if until:
        words.append("until=" + until)
    if typed:
        words.append("type=" + typed.hex())
    out, rc, err = m31(*words, "--", *argv)
    parts = out.split(" ")
    if len(parts) < 2 or parts[0] != "pty":
        return None, out + err
    try:
        return int(parts[1]), bytes.fromhex(parts[2] if len(parts) > 2 else "")
    except ValueError:
        return None, out

pty_cases = [
    ("tty and stty size, 31 by 97", ["sh", "-c", "tty; stty size; echo err >&2"], dict(rows=31, cols=97)),
    ("isatty on 0, 1 and 2", ["sh", "-c", "[ -t 0 ] && [ -t 1 ] && [ -t 2 ] && echo istty"], {}),
    ("controlling terminal: /dev/tty opens", ["sh", "-c", "echo viatty > /dev/tty"], {}),
    ("exit code 7", ["sh", "-c", "exit 7"], {}),
    ("interactive cat: echo, then ^D", ["cat"], dict(until=b"", typed=b"hello\nworld\n\x04")),
    ("raw mode passes ^C and ^D through", ["sh", "-c", "stty raw -echo; echo ready; head -c 8"], dict(until=b"ready", typed=b"abc\x03def\x04Z")),
    ("^C interrupts the child", ["sh", "-c", "echo ready; exec sleep 30"], dict(until=b"ready", typed=b"\x03")),
    ("a line is read once typed", ["sh", "-c", "printf 'name? '; read n; echo \"hi $n\""], dict(until=b"name? ", typed=b"ann\n")),
]
for label, argv, kw in pty_cases:
    until = kw.get("until", b"")
    want_code, want_out = pty_oracle(argv, rows=kw.get("rows", 24), cols=kw.get("cols", 80), until=until, typed=kw.get("typed", b""))
    got_code, got_out = m31_pty(argv, rows=kw.get("rows", 24), cols=kw.get("cols", 80), until=until.decode(), typed=kw.get("typed", b""))
    check("pty: " + label, got_code == want_code and norm(got_out) == norm(want_out), "got %r %r want %r %r" % (got_code, got_out, want_code, want_out))

# A resize is seen by the child: stty size before and after.
out, rc, err = m31("pty-resize")
want = ["first %s" % b"24 80\r\n".hex(), "resized true", "rest %s" % b"\r\n40 132\r\n".hex(), "status exited 0"]
check("pty: resize, seen by stty size", out.splitlines() == want, out + err)

out, rc, err = m31("pty", "--", "no-such-program-2026")
check("pty: a program that is not there", out == "pty error program or directory not found", out + err)

sys.exit(1 if bad else 0)
PY
done
if [ $failed -ne 0 ]; then echo "FAILED"; exit 1; fi
echo "proc: all checks passed"
